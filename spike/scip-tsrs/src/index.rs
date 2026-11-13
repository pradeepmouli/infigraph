use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, Result};
use protobuf::{Message, MessageField};
use scip::types::{
    Document, Index, Metadata, Occurrence, PositionEncoding, SymbolInformation, SymbolRole,
    TextEncoding, ToolInfo,
};
use serde_json::{Value, json};
use tsrs_ast::{self as ast, Kind, Node, SourceFile, SymbolFlags};
use tsrs_checker::ContextFlags;
use tsrs_compiler::Checker;
use tsrs_core::P;
use tsrs_scanner::{get_text_of_node, get_token_pos_of_node};

use crate::scip_io::{Range, encode_range};
use crate::{
    Project, declarations, describe_declarations, node_range, parents, source_range, visit,
};

const DEFINITION: i32 = SymbolRole::Definition as i32;
const ANONYMOUS_PACKAGE: &str = "scip-typescript npm . . ";

pub(super) fn write_index(
    project: &Project,
    checker: &mut Checker,
    output: &Path,
    mut trace: Option<BufWriter<File>>,
) -> Result<()> {
    let project_root = url::Url::from_directory_path(&project.root)
        .map_err(|_| anyhow::anyhow!("invalid SCIP root {}", project.root.display()))?;
    let mut output = BufWriter::new(File::create(output)?);
    Index {
        metadata: MessageField::some(Metadata {
            tool_info: MessageField::some(ToolInfo {
                name: "scip-tsrs-spike".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..ToolInfo::default()
            }),
            project_root: project_root.into(),
            text_document_encoding: TextEncoding::UTF8.into(),
            ..Metadata::default()
        }),
        ..Index::default()
    }
    .write_to_writer(&mut output)?;

    let mut names = Names {
        root: &project.root,
        paths: HashMap::new(),
        global: HashMap::new(),
    };
    for &file in &project.files {
        let first = get_token_pos_of_node(file.as_node(), file, false) as usize;
        if file.text().len() - first > 1_000_000 {
            eprintln!(
                "skipping {} above the oracle's default 1 MB limit",
                file.file_name()
            );
            continue;
        }
        let relative_path = Path::new(file.file_name())
            .strip_prefix(&project.root)?
            .to_str()
            .context("file path is not UTF-8")?
            .to_owned();
        let document = Document {
            language: if relative_path.ends_with(".tsx") {
                "TypeScriptReact"
            } else {
                "TypeScript"
            }
            .into(),
            relative_path,
            position_encoding: PositionEncoding::UTF16CodeUnitOffsetFromLineStart.into(),
            ..Document::default()
        };
        let indexer = FileIndexer {
            names: &mut names,
            checker,
            file,
            document,
            locals: HashMap::new(),
            property_counters: HashMap::new(),
            local_counter: 0,
            trace: trace.as_mut(),
        };
        let document = indexer.run()?;
        // Concatenating protobuf Index fragments appends their repeated Document fields.
        Index {
            documents: vec![document],
            ..Index::default()
        }
        .write_to_writer(&mut output)?;
    }
    if let Some(trace) = trace.as_mut() {
        trace.flush()?;
    }
    output.flush()?;
    Ok(())
}

struct Names<'a> {
    root: &'a Path,
    paths: HashMap<String, String>,
    global: HashMap<P<Node>, String>,
}

impl Names<'_> {
    fn for_path(&mut self, path: &str) -> String {
        if let Some(name) = self.paths.get(path) {
            return name.clone();
        }
        let path_obj = Path::new(path);
        let manifest = path_obj.join("package.json");
        let name = if let Some(lib) = path.strip_prefix("bundled:///libs/") {
            format!(
                "scip-typescript npm typescript 7.1.0-beta lib/{}/",
                escape(lib)
            )
        } else if fs::symlink_metadata(&manifest).is_ok_and(|metadata| metadata.is_file()) {
            let package: Option<Value> = fs::read(&manifest)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            package
                .as_ref()
                .and_then(|package| {
                    let name = package["name"].as_str()?;
                    let version = package["version"].as_str().unwrap_or("HEAD");
                    Some(format!("scip-typescript npm {name} {version} "))
                })
                .unwrap_or_else(|| ANONYMOUS_PACKAGE.into())
        } else if path_obj == self.root || path_obj.parent().is_none() {
            ANONYMOUS_PACKAGE.into()
        } else {
            let parent = path_obj.parent().unwrap().to_str().unwrap();
            let basename = path_obj.file_name().unwrap().to_str().unwrap();
            format!("{}{}/", self.for_path(parent), escape(basename))
        };
        self.paths.insert(path.to_owned(), name.clone());
        name
    }
}

struct FileIndexer<'a, 'root> {
    names: &'a mut Names<'root>,
    checker: &'a mut Checker,
    file: P<SourceFile>,
    document: Document,
    locals: HashMap<P<Node>, String>,
    property_counters: HashMap<String, usize>,
    local_counter: usize,
    trace: Option<&'a mut BufWriter<File>>,
}

impl FileIndexer<'_, '_> {
    fn run(mut self) -> Result<Document> {
        let module = self.name(self.file.as_node());
        self.emit(module, [0, 0, 0, 0], DEFINITION);
        let mut result = Ok(());
        visit(self.file.as_node(), &mut |node| {
            if result.is_ok()
                && (matches!(
                    node.kind(),
                    Kind::Identifier | Kind::PrivateIdentifier | Kind::Constructor
                ) || ast::is_string_literal_like(node))
            {
                result = self.visit_occurrence(node);
            }
        });
        result?;
        Ok(self.document)
    }

    fn visit_occurrence(&mut self, node: P<Node>) -> Result<()> {
        let Some(parent) = node.parent() else {
            return Ok(());
        };
        let (range, symbol) = if node.kind() == Kind::Constructor {
            let start = get_token_pos_of_node(node, self.file, false);
            if !self.file.text()[start as usize..].starts_with("constructor") {
                return Ok(());
            }
            let range = source_range(self.file, start, start + "constructor".len() as u32);
            (
                range,
                self.checker.get_symbol_of_declaration_exported(parent),
            )
        } else {
            (
                node_range(self.file, node),
                self.checker.get_symbol_at_location_exported(node),
            )
        };
        let trace_start = self.document.occurrences.len();
        let definition_declaration = [if node.kind() == Kind::Constructor {
            node
        } else {
            parent
        }];
        let mut chosen = &[][..];
        let mut route = "no checker symbol";
        if let Some(mut symbol) = symbol {
            if node.kind() == Kind::Identifier
                && symbol.flags().contains(SymbolFlags::Alias)
                && let Some(&first) = symbol.declarations().first()
                && (parent == first || skip_alias(first))
            {
                let target = self.checker.get_aliased_symbol(symbol);
                if !target.declarations().is_empty() {
                    symbol = target;
                }
            }
            let contextual = self.contextual_declarations(node);
            let definition = node.kind() == Kind::Constructor
                || (contextual.is_empty()
                    && is_definition_parent(parent.kind())
                    && parent.name() == Some(node));
            let nodes = if definition {
                route = "definition";
                &definition_declaration[..]
            } else if !contextual.is_empty() {
                route = "contextual property";
                contextual
            } else {
                route = "checker declarations";
                symbol.declarations()
            };
            for &decl in nodes {
                let mut name = self.name(decl);
                if name.is_empty() {
                    continue;
                }
                if decl.kind() == Kind::ClassDeclaration
                    && has_constructor(decl)
                    && is_new_target(node)
                {
                    name.push_str("`<constructor>`().");
                }
                self.emit(name, range, if definition { DEFINITION } else { 0 });
                if definition {
                    self.emit_related_references(node, decl, range);
                    break;
                }
            }
            chosen = nodes;
        }
        if let Some(trace) = self.trace.as_mut() {
            let token = if node.kind() == Kind::Constructor {
                "constructor".into()
            } else {
                get_text_of_node(node)
            };
            let emitted: Vec<_> = self.document.occurrences[trace_start..]
                .iter()
                .map(|occ| (&occ.symbol, occ.symbol_roles))
                .collect();
            writeln!(
                trace,
                "{}",
                json!({
                    "file": self.document.relative_path, "range": range, "token": token,
                    "kind": format!("{:?}", node.kind()), "parents": parents(node),
                    "checkerSymbol": symbol.map(|symbol| symbol.name()),
                    "checkerDeclarations": describe_declarations(declarations(symbol)),
                    "chosenDeclarations": describe_declarations(chosen),
                    "route": route, "emitted": emitted,
                })
            )?;
        }
        Ok(())
    }

    fn contextual_declarations(&mut self, node: P<Node>) -> &'static [P<Node>] {
        let Some(element) = object_member(node) else {
            return &[];
        };
        let object = element.parent().unwrap();
        let Some(name) = element.name().and_then(ast::try_get_text_of_property_name) else {
            return &[];
        };
        if name.is_empty() {
            return &[];
        }
        let property = self
            .checker
            .get_contextual_type_exported(object, ContextFlags::None)
            .and_then(|ty| self.checker.get_property_of_type_exported(ty, &name));
        declarations(property)
    }

    fn emit_related_references(&mut self, node: P<Node>, decl: P<Node>, range: Range) {
        let symbol = if decl.kind() == Kind::ShorthandPropertyAssignment {
            self.checker
                .get_shorthand_assignment_value_symbol(Some(decl))
        } else if node.kind() == Kind::Identifier
            && decl.kind() == Kind::BindingElement
            && let Some(pattern) = decl
                .parent()
                .filter(|p| p.kind() == Kind::ObjectBindingPattern)
        {
            let ty = self.checker.get_type_at_location(pattern);
            self.checker
                .get_property_of_type_exported(ty, &get_text_of_node(node))
        } else {
            None
        };
        for &related in declarations(symbol) {
            let name = self.name(related);
            if !name.is_empty() {
                self.emit(name, range, 0);
            }
        }
    }

    fn emit(&mut self, symbol: String, range: Range, role: i32) {
        let typed_range = Some(encode_range(range));
        if self.document.occurrences.last().is_some_and(|occ| {
            occ.symbol == symbol && occ.typed_range == typed_range && occ.symbol_roles == role
        }) {
            return;
        }
        if role == DEFINITION {
            self.document.symbols.push(SymbolInformation {
                symbol: symbol.clone(),
                ..SymbolInformation::default()
            });
        }
        self.document.occurrences.push(Occurrence {
            symbol,
            typed_range,
            symbol_roles: role,
            ..Occurrence::default()
        });
    }

    fn name(&mut self, node: P<Node>) -> String {
        if let Some(name) = self
            .names
            .global
            .get(&node)
            .or_else(|| self.locals.get(&node))
        {
            return name.clone();
        }
        let name = match node.kind() {
            Kind::Block => return String::new(),
            Kind::SourceFile => self.names.for_path(node.as_source_file().file_name()),
            Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment => {
                let key = get_text_of_node(node.name().unwrap());
                let counter = self.property_counters.entry(key.clone()).or_default();
                let descriptor = escape(&format!("{key}{counter}"));
                *counter += 1;
                let file = ast::get_source_file_of_node(node).unwrap();
                format!("{}{descriptor}:", self.name(file.as_node()))
            }
            _ => {
                let Some(parent) = node.parent() else {
                    return String::new();
                };
                let owner = self.name(parent);
                if owner.is_empty() || owner.starts_with("local ") {
                    return self.local(node);
                }
                if transparent(node) {
                    owner
                } else {
                    if matches!(
                        node.kind(),
                        Kind::ImportSpecifier | Kind::ImportClause | Kind::NamespaceImport
                    ) && let Some(target) = self.checker.get_type_at_location(node).symbol()
                        && let Some(&decl) = target.declarations().first()
                        && decl != node
                    {
                        return self.name(decl);
                    }
                    let Some(descriptor) = self.descriptor(node) else {
                        return self.local(node);
                    };
                    format!("{owner}{descriptor}")
                }
            }
        };
        self.names.global.insert(node, name.clone());
        name
    }

    fn local(&mut self, node: P<Node>) -> String {
        let name = format!("local {}", self.local_counter);
        self.local_counter += 1;
        self.locals.insert(node, name.clone());
        name
    }

    fn descriptor(&mut self, node: P<Node>) -> Option<String> {
        use Kind::*;
        let name = || node.name().map(|name| escape(&get_text_of_node(name)));
        Some(match node.kind() {
            InterfaceDeclaration | EnumDeclaration | TypeAliasDeclaration | ClassDeclaration
            | ClassExpression => format!("{}#", name()?),
            FunctionDeclaration | MethodSignature | MethodDeclaration => format!("{}().", name()?),
            Constructor => "`<constructor>`().".into(),
            PropertyDeclaration | PropertySignature | EnumMember | VariableDeclaration => {
                format!("{}.", name()?)
            }
            GetAccessor | SetAccessor => {
                let prefix = if node.kind() == GetAccessor {
                    "<get>"
                } else {
                    "<set>"
                };
                let text = get_text_of_node(node.name()?);
                format!("{}().", escape(&format!("{prefix}{text}")))
            }
            ModuleDeclaration => format!("{}/", name()?),
            Parameter => format!("({})", name()?),
            TypeParameter => format!("[{}]", name()?),
            TypeReference => format!(
                "{}:",
                escape(&get_text_of_node(node.as_type_reference_node().type_name()))
            ),
            TypeLiteral => {
                let ordinal = self.local_counter;
                self.local_counter += 1;
                format!("typeLiteral{ordinal}:")
            }
            _ => return None,
        })
    }
}

fn object_member(node: P<Node>) -> Option<P<Node>> {
    let parent = node.parent()?;
    let element =
        if ast::is_string_literal_like(node) && parent.kind() == Kind::ComputedPropertyName {
            parent.parent()?
        } else if matches!(
            node.kind(),
            Kind::Identifier | Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral
        ) && parent.name() == Some(node)
        {
            parent
        } else {
            return None;
        };
    (ast::is_object_literal_element(element)
        && element.parent()?.kind() == Kind::ObjectLiteralExpression)
        .then_some(element)
}

fn is_definition_parent(kind: Kind) -> bool {
    use Kind::*;
    matches!(
        kind,
        BindingElement
            | EnumDeclaration
            | EnumMember
            | VariableDeclaration
            | PropertyDeclaration
            | GetAccessor
            | SetAccessor
            | MethodSignature
            | MethodDeclaration
            | PropertySignature
            | FunctionDeclaration
            | ModuleDeclaration
            | PropertyAssignment
            | ShorthandPropertyAssignment
            | Parameter
            | TypeParameter
            | TypeAliasDeclaration
            | InterfaceDeclaration
            | ClassDeclaration
    )
}

fn transparent(node: P<Node>) -> bool {
    use Kind::*;
    matches!(
        node.kind(),
        ModuleBlock
            | ImportDeclaration
            | NamedImports
            | VariableStatement
            | VariableDeclarationList
    ) || (node.kind() == ImportClause && node.name().is_none())
}

fn skip_alias(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ImportClause | Kind::ImportEqualsDeclaration
    ) || (node.kind() == Kind::ImportSpecifier
        && node
            .parent()
            .is_some_and(|p| p.kind() == Kind::NamedImports))
}

fn has_constructor(class: P<Node>) -> bool {
    class
        .as_class_declaration()
        .members()
        .nodes()
        .iter()
        .any(|member| member.kind() == Kind::Constructor)
}

fn is_new_target(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        (node.kind() == Kind::Identifier && parent.kind() == Kind::NewExpression)
            || (parent.kind() == Kind::PropertyAccessExpression
                && parent
                    .parent()
                    .is_some_and(|parent| parent.kind() == Kind::NewExpression))
    })
}

fn escape(text: &str) -> String {
    if text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_$+-".contains(&byte))
    {
        text.to_owned()
    } else {
        format!("`{}`", text.replace('`', "``"))
    }
}
