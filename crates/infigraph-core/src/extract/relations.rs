use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::lang::CustomEdgeDef;
use crate::model::{Relation, RelationKind, Span};

/// Extract relationships from a parsed AST using a Tree-sitter query.
///
/// The query must use these capture names:
///   @call.func / @call.site          — function calls
///   @import.module / @import.name    — imports
///   @inherit.child / @inherit.parent — inheritance
///   @{custom}.source / @{custom}.target — custom edges (from language pack custom_edges)
pub fn extract_relations(file: &str, source: &[u8], root: Node, query: &Query) -> Vec<Relation> {
    extract_relations_with_custom_edges(file, source, root, query, &[])
}

/// Extract relationships including custom edge types defined by the language pack.
pub fn extract_relations_with_custom_edges(
    file: &str,
    source: &[u8],
    root: Node,
    query: &Query,
    custom_edges: &[CustomEdgeDef],
) -> Vec<Relation> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);

    let capture_names = query.capture_names();

    let mut relations = Vec::new();

    while let Some(m) = matches.next() {
        let mut rel_kind = None;
        let mut source_name = None;
        let mut target_name = None;
        let mut site_node = None;
        let mut receiver_text = None;
        // For custom edges: track source/target per capture prefix
        let mut custom_source: Option<(String, String)> = None; // (edge_name, source_text)
        let mut custom_target: Option<(String, String)> = None; // (edge_name, target_text)
        let mut custom_site_node: Option<Node> = None;
        let mut custom_edge_name: Option<String> = None;

        for capture in m.captures {
            let idx = capture.index as usize;
            let cap_name = capture_names[idx];
            let node = capture.node;
            let text = node_text(node, source);

            match cap_name {
                "call.func" => {
                    target_name = Some(text);
                    rel_kind = Some(RelationKind::Calls);
                }
                "call.site" => {
                    site_node = Some(node);
                }
                "call.caller" => {
                    source_name = Some(text);
                }
                "call.receiver" => {
                    receiver_text = Some(text);
                }
                "import.module" => {
                    target_name = Some(text);
                    rel_kind = Some(RelationKind::Imports);
                    source_name = Some(file.to_string());
                }
                "import.name" => {
                    target_name = Some(text);
                    rel_kind = Some(RelationKind::Imports);
                    source_name = Some(file.to_string());
                }
                "inherit.child" => {
                    source_name = Some(text);
                    if rel_kind.is_none() {
                        rel_kind = Some(RelationKind::Inherits);
                    }
                }
                "inherit.parent" => {
                    target_name = Some(text);
                    rel_kind = Some(RelationKind::Inherits);
                }
                other => {
                    // Check for custom edge captures: "{capture}.source", "{capture}.target",
                    // or "{capture}.site" (used to infer source from enclosing function)
                    if let Some((prefix, suffix)) = other.split_once('.') {
                        if let Some(edge_def) = custom_edges.iter().find(|e| e.capture == prefix) {
                            custom_edge_name = Some(edge_def.name.clone());
                            match suffix {
                                "source" => {
                                    custom_source = Some((edge_def.name.clone(), text));
                                    custom_site_node = Some(node);
                                }
                                "target" => {
                                    custom_target = Some((edge_def.name.clone(), text));
                                }
                                "site" => {
                                    custom_site_node = Some(node);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        // Handle custom edge if we have a target (source can be inferred)
        if let Some((_, tgt_text)) = custom_target {
            let edge_name = if let Some((name, _)) = &custom_source {
                name.clone()
            } else {
                custom_edge_name.unwrap_or_default()
            };

            if edge_name.is_empty() {
                // No edge name resolved — skip
            } else {
                let src_text = if let Some((_, src)) = custom_source {
                    src
                } else if let Some(site) = custom_site_node {
                    // No explicit source — infer from enclosing function
                    find_enclosing_function(site, source).unwrap_or_else(|| file.to_string())
                } else {
                    file.to_string()
                };

                let span = custom_site_node.map(|n| Span {
                    file: file.to_string(),
                    start_line: n.start_position().row as u32 + 1,
                    start_col: n.start_position().column as u32,
                    end_line: n.end_position().row as u32 + 1,
                    end_col: n.end_position().column as u32,
                });

                let source_id = format!("{}::{}", file, src_text);
                let target_id = format!("{}::{}", file, tgt_text);

                relations.push(Relation {
                    source_id,
                    target_id,
                    kind: RelationKind::Custom(edge_name),
                    span,
                    receiver: None,
                });
                continue;
            }
        }

        if rel_kind == Some(RelationKind::Calls) && source_name.is_none() {
            if let Some(site) = site_node {
                source_name =
                    find_enclosing_function(site, source).or_else(|| Some(file.to_string()));
            }
        }

        // For self/this calls, resolve receiver to enclosing class name
        if rel_kind == Some(RelationKind::Calls) {
            if let Some(ref recv) = receiver_text {
                if recv == "self" || recv == "this" || recv == "@" {
                    if let Some(site) = site_node {
                        if let Some(cls) = find_enclosing_class(site, source) {
                            receiver_text = Some(cls);
                        }
                    }
                }
            }
        }

        if let (Some(kind), Some(src), Some(tgt)) = (rel_kind, source_name, target_name) {
            let span = site_node.map(|n| Span {
                file: file.to_string(),
                start_line: n.start_position().row as u32 + 1,
                start_col: n.start_position().column as u32,
                end_line: n.end_position().row as u32 + 1,
                end_col: n.end_position().column as u32,
            });

            let source_id = if kind == RelationKind::Imports {
                src
            } else {
                format!("{}::{}", file, src)
            };
            let target_id = format!("{}::{}", file, tgt);

            relations.push(Relation {
                source_id,
                target_id,
                kind,
                span,
                receiver: receiver_text.clone(),
            });
        }
    }

    relations
}

/// Walk up the AST to find the enclosing function/method definition and return its name.
fn find_enclosing_function(node: Node, source: &[u8]) -> Option<String> {
    let func_kinds = [
        "function_definition",  // Python, JS, Lua, VB6 Function
        "function_item",        // Rust
        "function_declaration", // Go, JS, TS, Java
        "method_declaration",   // Go, Java
        "method_definition",    // JS/TS class methods
        "func_literal",         // Go anonymous
        "sub_definition",       // VB6 Sub
        "property_definition",  // VB6 Property Get/Let/Set
    ];
    let sql_container_kinds = [
        "create_table", // SQL: CREATE TABLE ... AS SELECT
        "insert",       // SQL: INSERT INTO ... SELECT
    ];
    let mut current = node.parent();
    while let Some(n) = current {
        if func_kinds.contains(&n.kind()) {
            if let Some(name_node) = n.child_by_field_name("name") {
                return Some(node_text(name_node, source));
            }
        }
        // Pascal: defProc → header (declProc) → name
        // name may be identifier (bare) or genericDot (TClass.Method) — use rightmost identifier
        if n.kind() == "defProc" {
            if let Some(header) = n.child_by_field_name("header") {
                if let Some(name_node) = header.child_by_field_name("name") {
                    if name_node.kind() == "genericDot" {
                        if let Some(rhs) = name_node.child_by_field_name("rhs") {
                            return Some(node_text(rhs, source));
                        }
                    }
                    return Some(node_text(name_node, source));
                }
            }
        }
        if sql_container_kinds.contains(&n.kind()) {
            if let Some(obj_ref) = n.child_by_field_name("name") {
                return Some(node_text(obj_ref, source));
            }
            // Fallback: find first object_reference child
            let mut i = 0;
            while let Some(child) = n.child(i) {
                if child.kind() == "object_reference" {
                    if let Some(id) = child.child_by_field_name("name") {
                        return Some(node_text(id, source));
                    }
                }
                i += 1;
            }
        }
        if n.kind() == "cte" {
            // CTE: first child is identifier
            if let Some(id) = n.child(0) {
                if id.kind() == "identifier" {
                    return Some(node_text(id, source));
                }
            }
        }
        current = n.parent();
    }
    None
}

/// Walk up the AST to find the enclosing class/struct/impl and return its name.
fn find_enclosing_class(node: Node, source: &[u8]) -> Option<String> {
    let class_kinds = [
        "class_definition",  // Python
        "class_declaration", // Java, TS, JS, C#, Kotlin, Swift
        "class",             // Ruby
        "class_specifier",   // C/C++
        "impl_item",         // Rust
        "struct_item",       // Rust
        "defmodule",         // Elixir
    ];
    let mut current = node.parent();
    while let Some(n) = current {
        if class_kinds.contains(&n.kind()) {
            if let Some(name_node) = n.child_by_field_name("name") {
                return Some(node_text(name_node, source));
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
        current = n.parent();
    }
    None
}

fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}
