use sha2::{Digest, Sha256};
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::analysis::cyclomatic_complexity;
use crate::model::{Span, Symbol, SymbolKind};

/// Extract symbols from a parsed AST using a Tree-sitter query.
///
/// The query must use these capture names:
///   @func.def / @func.name / @func.docstring / @func.decorator
///   @method.def / @method.name / @method.docstring / @method.decorator
///   @class.def / @class.name / @class.docstring / @class.decorator
///   @module.def / @module.name
///   @test.def / @test.name / @test.docstring
///   @var.def / @var.name
///   @route.def / @route.method / @route.path / @route.handler
pub fn extract_entities(
    file: &str,
    source: &[u8],
    root: Node,
    query: &Query,
    language: &str,
) -> Vec<Symbol> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);

    let capture_names = query.capture_names();

    let mut symbols = Vec::new();

    while let Some(m) = matches.next() {
        let mut name_text = None;
        let mut def_node = None;
        let mut kind = None;
        let mut docstring = None;
        let mut decorator = None;
        let mut route_method: Option<String> = None;
        let mut route_path: Option<String> = None;
        let mut route_handler: Option<String> = None;
        let mut route_def_node: Option<Node> = None;

        for capture in m.captures {
            let idx = capture.index as usize;
            let cap_name = capture_names[idx];
            let node = capture.node;

            match cap_name {
                "func.name" => {
                    name_text = Some(node_text(node, source));
                    if kind.is_none() {
                        kind = Some(SymbolKind::Function);
                    }
                }
                "func.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Function);
                    }
                }
                "func.docstring" => {
                    docstring = Some(strip_docstring(&node_text(node, source)));
                }
                "func.decorator" => {
                    decorator = Some(node_text(node, source));
                }
                "method.name" => {
                    name_text = Some(node_text(node, source));
                    kind = Some(SymbolKind::Method);
                }
                "method.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Method);
                    }
                }
                "method.docstring" => {
                    docstring = Some(strip_docstring(&node_text(node, source)));
                }
                "method.decorator" => {
                    decorator = Some(node_text(node, source));
                }
                "class.name" => {
                    name_text = Some(node_text(node, source));
                    if kind.is_none() {
                        kind = Some(SymbolKind::Class);
                    }
                }
                "class.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Class);
                    }
                }
                "class.docstring" => {
                    docstring = Some(strip_docstring(&node_text(node, source)));
                }
                "class.decorator" => {
                    decorator = Some(node_text(node, source));
                }
                "module.name" => {
                    let raw = node_text(node, source);
                    name_text = Some(strip_string_delimiters(&raw));
                    if kind.is_none() {
                        kind = Some(SymbolKind::Module);
                    }
                }
                "module.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Module);
                    }
                }
                "test.name" => {
                    name_text = Some(node_text(node, source));
                    kind = Some(SymbolKind::Test);
                }
                "test.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Test);
                    }
                }
                "test.docstring" => {
                    docstring = Some(strip_docstring(&node_text(node, source)));
                }
                "var.name" => {
                    name_text = Some(node_text(node, source));
                    if kind.is_none() {
                        kind = Some(SymbolKind::Variable);
                    }
                }
                "var.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Variable);
                    }
                }
                "section.name" => {
                    name_text = Some(node_text(node, source));
                    if kind.is_none() {
                        kind = Some(SymbolKind::Section);
                    }
                }
                "section.def" => {
                    def_node = Some(node);
                    if kind.is_none() {
                        kind = Some(SymbolKind::Section);
                    }
                }
                "route.method" => {
                    route_method = Some(node_text(node, source));
                }
                "route.path" => {
                    route_path = Some(strip_string_delimiters(&node_text(node, source)));
                }
                "route.handler" => {
                    route_handler = Some(node_text(node, source));
                }
                "route.def" => {
                    route_def_node = Some(node);
                }
                _ => {}
            }
        }

        // Prepend decorator/attribute text to docstring for searchability
        // If no decorator from query capture, try AST-based extraction (Rust attrs, Go comments, C# attrs)
        if decorator.is_none() {
            if let Some(node) = def_node {
                decorator = find_preceding_attributes(node, source);
            }
        }
        if let Some(dec) = decorator {
            let dec_clean = dec.trim().to_string();
            docstring = Some(match docstring {
                Some(doc) => format!("{} {}", dec_clean, doc),
                None => dec_clean,
            });
        }

        if let (Some(name), Some(node), Some(sym_kind)) = (name_text, def_node, kind) {
            let span = Span {
                file: file.to_string(),
                start_line: node.start_position().row as u32 + 1,
                start_col: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_col: node.end_position().column as u32,
            };

            let signature_hash = hash_node(node, source);

            // Find parent class for methods by walking up the AST
            let parent_class = find_parent_class(node, source);
            let id = if let Some(ref cls) = parent_class {
                format!("{}::{}::{}", file, cls, name)
            } else {
                format!("{}::{}", file, name)
            };
            let parent = parent_class.map(|cls| format!("{}::{}", file, cls));

            let complexity = match sym_kind {
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test =>
                    cyclomatic_complexity(node),
                _ => 1,
            };

            symbols.push(Symbol {
                id,
                name,
                kind: sym_kind,
                span,
                signature_hash,
                parent,
                language: language.to_string(),
                visibility: None,
                docstring,
                complexity,
            });
        }

        // Create Route symbol from @route.* captures
        if let Some(path) = route_path {
            let method = route_method.unwrap_or_default().to_uppercase();
            let handler = route_handler.clone().unwrap_or_default();
            let route_name = if method.is_empty() {
                format!("ROUTE {}", path)
            } else {
                format!("{} {}", method, path)
            };
            let node = route_def_node.unwrap_or(def_node.unwrap_or(root));
            let span = Span {
                file: file.to_string(),
                start_line: node.start_position().row as u32 + 1,
                start_col: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_col: node.end_position().column as u32,
            };
            let id = format!("{}::{}", file, route_name.replace(' ', "_").replace('/', "_"));
            let docstring = if handler.is_empty() {
                Some(format!("route {} {}", method, path))
            } else {
                Some(format!("route {} {} handler={}", method, path, handler))
            };
            symbols.push(Symbol {
                id,
                name: route_name,
                kind: SymbolKind::Route,
                span,
                signature_hash: hash_node(node, source),
                parent: None,
                language: language.to_string(),
                visibility: None,
                docstring,
                complexity: 1,
            });
        }
    }

    // Deduplicate by ID — prefer more specific kind (Test > Function)
    let mut seen = std::collections::HashMap::new();
    for sym in symbols {
        seen.entry(sym.id.clone())
            .and_modify(|existing: &mut Symbol| {
                // Test is more specific than Function
                if sym.kind == SymbolKind::Test && existing.kind == SymbolKind::Function {
                    *existing = sym.clone();
                }
            })
            .or_insert(sym);
    }
    seen.into_values().collect()
}

/// Walk up the AST to find the enclosing class_definition and return its name.
fn find_parent_class(node: Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "class_definition" {
            // The name child of a class_definition is the class name
            return n.child_by_field_name("name").map(|name_node| node_text(name_node, source));
        }
        current = n.parent();
    }
    None
}

/// Look at preceding siblings for attribute/decorator nodes.
/// Handles: Rust `attribute_item` (#[get("/path")]), C# `attribute_list` ([HttpGet]),
/// Go preceding line comments (// @router /api/users [get]), and similar patterns.
fn find_preceding_attributes(node: Node, source: &[u8]) -> Option<String> {
    // Node kinds that represent decorators/attributes across languages
    const ATTR_KINDS: &[&str] = &[
        "attribute_item",   // Rust: #[get("/path")]
        "attribute_list",   // C#: [HttpGet], PHP 8: #[Route("/path")]
        "attribute",        // C# inner, PHP inner
        "annotation",       // Kotlin, Scala, Java (fallback)
        "decorator",        // TypeScript/JS (NestJS @Controller, @Get)
        "marker_annotation", // Java @Override, @GetMapping
    ];

    // Comment kinds that may contain route annotations (Go swagger, JSDoc)
    const COMMENT_KINDS: &[&str] = &["comment", "line_comment", "block_comment"];

    let mut attrs = Vec::new();

    // Collect from preceding siblings
    collect_attrs(node, source, ATTR_KINDS, COMMENT_KINDS, &mut attrs);

    // Also check parent's preceding siblings (for attributes at different nesting)
    if attrs.is_empty() {
        if let Some(parent) = node.parent() {
            collect_attrs(parent, source, ATTR_KINDS, COMMENT_KINDS, &mut attrs);
        }
    }

    if attrs.is_empty() {
        None
    } else {
        attrs.reverse();
        Some(attrs.join(" "))
    }
}

/// Collect attribute/decorator nodes from preceding siblings.
fn collect_attrs(
    node: Node,
    source: &[u8],
    attr_kinds: &[&str],
    comment_kinds: &[&str],
    attrs: &mut Vec<String>,
) {
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        if attr_kinds.contains(&sib.kind()) {
            attrs.push(node_text(sib, source));
            sibling = sib.prev_sibling();
        } else if comment_kinds.contains(&sib.kind()) {
            // Only capture annotation-like comments: // @Router, /// @route, # @app.route
            let text = node_text(sib, source);
            if text.contains("@") || text.contains("route") || text.contains("endpoint")
                || text.contains("handler") || text.contains("API")
            {
                attrs.push(text);
            }
            sibling = sib.prev_sibling();
        } else {
            break;
        }
    }
}

fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

fn hash_node(node: Node, source: &[u8]) -> String {
    let mut hasher = Sha256::new();
    let text = &source[node.byte_range()];
    hasher.update(text);
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Strip string delimiters (quotes) from a captured path string.
fn strip_string_delimiters(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let s = s.strip_prefix('\'').unwrap_or(s);
    let s = s.strip_suffix('\'').unwrap_or(s);
    let s = s.strip_prefix('`').unwrap_or(s);
    let s = s.strip_suffix('`').unwrap_or(s);
    s.to_string()
}

/// Strip triple-quote delimiters and leading whitespace from a docstring.
fn strip_docstring(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("\"\"\"").or_else(|| s.strip_prefix("'''")).unwrap_or(s);
    let s = s.strip_suffix("\"\"\"").or_else(|| s.strip_suffix("'''")).unwrap_or(s);
    // Dedent: find minimum indentation and strip it
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= 1 {
        return s.trim().to_string();
    }
    let min_indent = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            result.push_str(line.trim());
        } else if line.len() >= min_indent {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&line[min_indent..]);
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line.trim());
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    #[test]
    fn test_module_capture_produces_module_symbol() {
        // Use Python grammar: capture identifier as @module.name and enclosing
        // assignment as @module.def — two distinct nodes, proving arm independence.
        let grammar = tree_sitter_python::LANGUAGE.into();
        let src = b"MyModule = 1";
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();

        // identifier node (name) and assignment node (def) are distinct
        let query = tree_sitter::Query::new(
            &grammar,
            r#"(assignment left: (identifier) @module.name) @module.def"#,
        ).unwrap();

        let symbols = extract_entities("test.bas", src, root, &query, "vb6");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].kind, crate::model::SymbolKind::Module);
        assert_eq!(symbols[0].name, "MyModule");
    }
}
