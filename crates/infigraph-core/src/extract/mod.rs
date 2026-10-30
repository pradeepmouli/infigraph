mod entities;
mod members;
mod relations;
pub use entities::extract_entities;
pub use members::{literal_members, LiteralMember};
pub use relations::{extract_relations, extract_relations_with_custom_edges};

use anyhow::Result;
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::analysis::extract_statements;
use crate::lang::{LanguagePack, ParserBackend};
use crate::model::{FileExtraction, Relation, RelationKind, Span, Statement, SymbolKind};

pub(super) fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

/// Descend through declarator wrappers (pointer/reference return types, e.g.
/// `const char* Class::method()`) to find a `qualified_identifier` — the
/// `Class::method` name node in an out-of-line C++ method definition.
pub(super) fn find_qualified_identifier(node: Node) -> Option<Node> {
    if node.kind() == "qualified_identifier" {
        return Some(node);
    }
    if node.kind() == "function_declarator" {
        return node
            .child_by_field_name("declarator")
            .and_then(find_qualified_identifier);
    }
    node.child_by_field_name("declarator")
        .and_then(find_qualified_identifier)
}

/// Node kinds that own the members declared inside them. The one list: the
/// owner walk and the field-type lookup in `relations` both ask it.
pub(super) const CLASS_KINDS: &[&str] = &[
    "class_definition",           // Python
    "class_declaration",          // Java, TS, JS, C#, Kotlin, Swift
    "abstract_class_declaration", // TS, TSX
    "class",                      // Ruby
    "class_specifier",            // C/C++
    "struct_specifier",           // C/C++ struct
    "impl_item",                  // Rust
    "struct_item",                // Rust
    "defmodule",                  // Elixir
];

/// Walk up the AST to find the enclosing class/impl/namespace and return its name.
///
/// Consolidated from what used to be two independently-drifted copies
/// (`entities.rs`'s `find_parent_class`, `relations.rs`'s `find_enclosing_class`) —
/// one had struct_specifier and the C++ out-of-line-method branch, the other had
/// Elixir's `defmodule` and Pascal's `declClass`/`declIntf`. This is the union of
/// both, so both extraction passes now use the same, correctly-scoped answer.
pub(super) fn find_parent_class(
    node: Node,
    source: &[u8],
    decompose_query: Option<&Query>,
) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        // Rust's impl_item has no "name" field (only body/trait/type/type_parameters —
        // see the symbol-identity-and-scoping-hardening spec's Finding 3) — resolve
        // its "type" field instead, through the same decompose mechanism entities.scm's
        // @method.parent capture uses, so a generic `impl<T> Foo<T> for Vec<Bar>` still
        // bottoms out at "Bar" rather than returning None or a raw compound string.
        if n.kind() == "impl_item" {
            if let Some(type_node) = n.child_by_field_name("type") {
                return Some(resolve_compound_node_text(
                    type_node,
                    source,
                    decompose_query,
                ));
            }
        } else if CLASS_KINDS.contains(&n.kind()) {
            if let Some(name_node) = n.child_by_field_name("name") {
                return Some(node_text(name_node, source));
            }
        }
        if n.kind() == "namespace_definition" {
            if let Some(name_node) = n.child_by_field_name("name") {
                return Some(node_text(name_node, source));
            }
        }
        if n.kind() == "function_definition" {
            if let Some(declarator) = n.child_by_field_name("declarator") {
                if let Some(qualified) = find_qualified_identifier(declarator) {
                    if let Some(scope) = qualified.child_by_field_name("scope") {
                        return Some(node_text(scope, source));
                    }
                }
            }
        }
        // Pascal: declClass/declIntf is child of declType which has the name
        if n.kind() == "declClass" || n.kind() == "declIntf" {
            if let Some(parent) = n.parent() {
                if parent.kind() == "declType" {
                    if let Some(name_node) = parent.child_by_field_name("name") {
                        return Some(node_text(name_node, source));
                    }
                }
            }
        }
        // Protobuf: an `rpc` lives inside a `service` node. The service name has
        // no "name" field — it's a `service_name` child. Without this, proto RPC
        // methods get an empty parent, so gRPC contract extraction (which groups
        // RPCs under their service) finds nothing (AIF3X-331 #21).
        if n.kind() == "service" {
            let mut cursor = n.walk();
            let name = n
                .children(&mut cursor)
                .find(|c| c.kind() == "service_name" || c.kind() == "message_name")
                .map(|name_node| node_text(name_node, source));
            if let Some(name) = name {
                return Some(name);
            }
        }
        current = n.parent();
    }
    None
}

/// Whether the binding named `name` in `node` (a `variable_declarator`, or the
/// `lexical_declaration` holding it) is given a function. Such a binding is
/// captured twice, as a function and as a variable, and the two are one symbol
/// only while they share an id, so it keeps the function's (the class walk).
pub(super) fn ts_binds_function(node: Node, name: &str, source: &[u8]) -> bool {
    let is_function = |declarator: Node| {
        declarator.child_by_field_name("value").is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function" | "generator_function"
            )
        })
    };
    if node.kind() == "variable_declarator" {
        return is_function(node);
    }
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).any(|d| {
        d.kind() == "variable_declarator"
            && d.child_by_field_name("name")
                .is_some_and(|n| node_text(n, source) == name)
            && is_function(d)
    });
    found
}

/// The owner path of a TypeScript/TSX variable or property, `None` when
/// nothing owns it. The path is the id's middle (`file::<path>::name`).
///
/// - A property of an interface or type literal is owned by the declaration
///   that holds it, through the properties between: `EmitConfig::nested` owns
///   `inner` in `interface EmitConfig { nested: { inner: number } }`, the name
///   scip-typescript gives it, so SCIP enriches the same symbol. A class field
///   is owned by its class. A type literal in a parameter or return type is
///   owned by nothing (no symbol, as `is_parameter_symbol` does for SCIP).
/// - A variable declared inside a function is owned by the nearest enclosing
///   *named* function -- a function or method declaration, or a function
///   expression bound to a variable or field -- under the id that function's
///   own symbol has (`Class::method`): an anonymous callback falls to the
///   named function around it. A module-level variable has no owner.
///
/// The class part is `find_parent_class`'s answer; this adds the kinds it
/// does not own: interfaces and aliases (only here, since other languages'
/// interface members keep their ids), functions, and type literals.
pub(super) fn ts_member_owner(node: Node, source: &[u8]) -> Option<String> {
    let name_of = |n: Node| {
        n.child_by_field_name("name")
            .filter(|name| {
                matches!(
                    name.kind(),
                    "identifier" | "type_identifier" | "property_identifier"
                )
            })
            .map(|name| node_text(name, source))
    };
    // names of properties between the node and its declaration, innermost first
    let mut between: Vec<String> = Vec::new();
    let mut in_type_literal = false;
    let mut current = node.parent();
    while let Some(n) = current {
        let base = match n.kind() {
            "object_type" => {
                in_type_literal = true;
                None
            }
            "property_signature" | "public_field_definition" => {
                between.extend(name_of(n));
                None
            }
            "interface_declaration" | "type_alias_declaration" => name_of(n),
            kind if CLASS_KINDS.contains(&kind) => name_of(n),
            // a parameter's inline type is no symbol, as for SCIP
            "required_parameter" | "optional_parameter" if in_type_literal => return None,
            // `const table: { rows: number }`: a module-level binding owns what
            // its type says; a local's type is no symbol (SCIP's is `local N`)
            "variable_declarator" if in_type_literal => {
                if ts_member_owner(n, source).is_some() {
                    return None;
                }
                name_of(n)
            }
            "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "function_expression"
            | "generator_function"
            | "arrow_function" => {
                let name = name_of(n).or_else(|| {
                    n.parent()
                        .filter(|p| {
                            matches!(p.kind(), "variable_declarator" | "public_field_definition")
                        })
                        .and_then(name_of)
                });
                name.map(|name| match find_parent_class(n, source, None) {
                    Some(class) => format!("{class}::{name}"),
                    None => name,
                })
            }
            _ => None,
        };
        if let Some(base) = base {
            between.reverse();
            return Some(
                std::iter::once(base)
                    .chain(between)
                    .collect::<Vec<_>>()
                    .join("::"),
            );
        }
        current = n.parent();
    }
    None
}

/// The TypeScript namespaces a node is declared in, outermost first:
/// `namespace Outer.Inner { … }` is `Outer` then `Inner`. scip-typescript names
/// a member of a namespace under the namespace, so its id has them too.
pub(super) fn ts_namespace_owners(node: Node, source: &[u8]) -> Vec<String> {
    let mut declared: Vec<Vec<String>> = Vec::new();
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "internal_module" {
            if let Some(name) = n.child_by_field_name("name") {
                declared.push(
                    node_text(name, source)
                        .split('.')
                        .map(str::to_string)
                        .collect(),
                );
            }
        }
        current = n.parent();
    }
    declared.into_iter().rev().flatten().collect()
}

/// Puts a TypeScript node's namespaces in front of its owner (`find_parent_class`'s
/// answer, or `ts_member_owner`'s): `namespace N { class C { m() {} } }` names
/// `m` as `N::C::m`, and a namespace-level `f` as `N::f`.
pub(super) fn ts_with_namespaces(
    node: Node,
    source: &[u8],
    owner: Option<String>,
) -> Option<String> {
    let mut path = ts_namespace_owners(node, source);
    if path.is_empty() {
        return owner;
    }
    path.extend(owner);
    Some(path.join("::"))
}

/// Resolve a captured compound node (generic type, qualified/scoped identifier,
/// member expression) down to its base identifier text. If `decompose_query` is
/// present, iteratively re-applies it to descend through wrapper nodes (e.g.
/// `generic_type`'s `type:` field, `scoped_type_identifier`'s `name:` field)
/// until it bottoms out at a plain identifier; otherwise returns the node's own
/// text unchanged. Shared by relation extraction (`@inherit.parent`/`@inherit.child`)
/// and entity extraction (e.g. Rust's `@method.parent` on a compound impl type).
pub(super) fn resolve_compound_node_text(
    node: Node,
    source: &[u8],
    decompose_query: Option<&Query>,
) -> String {
    let Some(query) = decompose_query else {
        return node_text(node, source);
    };

    let mut current = node;
    loop {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, current, source);
        let mut next = None;
        'outer: while let Some(m) = matches.next() {
            for capture in m.captures {
                if capture.node.parent().map(|p| p.id()) == Some(current.id()) {
                    next = Some(capture.node);
                    break 'outer;
                }
            }
        }
        match next {
            Some(n) => current = n,
            None => return node_text(current, source),
        }
    }
}

thread_local! {
    static TS_PARSER: std::cell::RefCell<tree_sitter::Parser> = std::cell::RefCell::new(tree_sitter::Parser::new());
}

/// Parse a source file and extract all symbols and relationships.
pub fn extract_file(path: &str, source: &[u8], pack: &LanguagePack) -> Result<FileExtraction> {
    let (symbols, mut relations, statements) = match &pack.backend {
        ParserBackend::TreeSitter {
            grammar,
            entity_query,
            relation_query,
            inherit_decompose_query,
        } => TS_PARSER.with(|cell| -> Result<_> {
            let mut parser = cell.borrow_mut();
            parser.set_language(grammar)?;

            let tree = parser
                .parse(source, None)
                .ok_or_else(|| anyhow::anyhow!("failed to parse {}", path))?;

            let root = tree.root_node();

            let symbols = extract_entities(
                path,
                source,
                root,
                entity_query,
                &pack.name,
                inherit_decompose_query.as_deref(),
            );
            let relations = if pack.custom_edges.is_empty() {
                extract_relations(
                    path,
                    source,
                    root,
                    relation_query,
                    inherit_decompose_query.as_deref(),
                )
            } else {
                extract_relations_with_custom_edges(
                    path,
                    source,
                    root,
                    relation_query,
                    &pack.custom_edges,
                    inherit_decompose_query.as_deref(),
                )
            };
            let stmts = extract_statements_for_symbols(root, source, &symbols);
            Ok((symbols, relations, stmts))
        })?,
        ParserBackend::Custom(extractor) => {
            let (s, r) = extractor.extract(path, source, &pack.name)?;
            (s, r, Vec::new())
        }
    };

    // Generate CALLS edges from Route symbols to their handler functions
    generate_route_handler_edges(path, &symbols, &mut relations);

    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(source);
        format!("{:x}", hasher.finalize())
    };

    Ok(FileExtraction {
        file: path.to_string(),
        language: pack.name.clone(),
        content_hash,
        symbols,
        relations,
        statements,
    })
}

fn extract_statements_for_symbols(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    symbols: &[crate::model::Symbol],
) -> Vec<Statement> {
    let fn_symbols: Vec<(&str, u32, u32)> = symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            )
        })
        .map(|s| (s.id.as_str(), s.span.start_line, s.span.end_line))
        .collect();

    if fn_symbols.is_empty() {
        return Vec::new();
    }

    let mut all_stmts = Vec::new();
    collect_fn_nodes(root, source, &fn_symbols, &mut all_stmts);
    let mut seen = std::collections::HashSet::new();
    all_stmts.retain(|s| seen.insert(s.id.clone()));
    all_stmts
}

fn collect_fn_nodes<'a>(
    node: tree_sitter::Node<'a>,
    source: &'a [u8],
    fn_symbols: &[(&str, u32, u32)],
    stmts: &mut Vec<Statement>,
) {
    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;

    if let Some((sym_id, _, _)) = fn_symbols
        .iter()
        .find(|(_, sl, el)| start == *sl && end == *el)
    {
        let mut extracted = extract_statements(node, source, sym_id, "");
        stmts.append(&mut extracted);
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_fn_nodes(child, source, fn_symbols, stmts);
        }
    }
}

/// Create CALLS relations from Route symbols to handler functions in the same file.
/// Matches route handler names from docstrings OR route names containing function names.
fn generate_route_handler_edges(
    file: &str,
    symbols: &[crate::model::Symbol],
    relations: &mut Vec<Relation>,
) {
    // Collect function/method names for matching
    let functions: Vec<(&str, &str)> = symbols
        .iter()
        .filter(|s| {
            (s.kind == SymbolKind::Function || s.kind == SymbolKind::Method) && s.span.file == file
        })
        .map(|s| (s.name.as_str(), s.id.as_str()))
        .collect();

    for sym in symbols {
        if sym.kind != SymbolKind::Route {
            continue;
        }

        let mut target_id: Option<String> = None;

        // Method 1: explicit handler= in docstring
        if let Some(doc) = &sym.docstring {
            if let Some(handler_name) = doc.split("handler=").nth(1).map(|h| h.trim()) {
                target_id = functions
                    .iter()
                    .find(|(name, _)| *name == handler_name)
                    .map(|(_, id)| id.to_string());
            }
        }

        // Method 2: Route is on the same line range as a function — check for overlap
        if target_id.is_none() {
            target_id = symbols
                .iter()
                .find(|s| {
                    (s.kind == SymbolKind::Function || s.kind == SymbolKind::Method)
                        && s.span.file == file
                        && s.span.start_line <= sym.span.end_line
                        && s.span.end_line >= sym.span.start_line
                })
                .map(|s| s.id.clone());
        }

        if let Some(tid) = target_id {
            if tid != sym.id {
                relations.push(Relation {
                    source_id: sym.id.clone(),
                    target_id: tid,
                    kind: RelationKind::Calls,
                    span: Some(Span {
                        file: file.to_string(),
                        start_line: sym.span.start_line,
                        start_col: sym.span.start_col,
                        end_line: sym.span.end_line,
                        end_col: sym.span.end_col,
                    }),
                    receiver: None,
                });
            }
        }
    }
}
