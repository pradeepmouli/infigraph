use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::model::{Relation, RelationKind, Span};

/// Extract relationships from a parsed AST using a Tree-sitter query.
///
/// The query must use these capture names:
///   @call.func / @call.site          — function calls
///   @import.module / @import.name    — imports
///   @inherit.child / @inherit.parent — inheritance
pub fn extract_relations(file: &str, source: &[u8], root: Node, query: &Query) -> Vec<Relation> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source);

    let capture_names = query.capture_names();

    let mut relations = Vec::new();

    while let Some(m) = matches.next() {
        let mut rel_kind = None;
        let mut source_name = None;
        let mut target_name = None;
        let mut site_node = None;

        for capture in m.captures {
            let idx = capture.index as usize;
            let cap_name = capture_names[idx];
            let node = capture.node;
            let text = node_text(node, source);

            match cap_name {
                // Function calls: the caller is the enclosing function, target is the called name
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
                // Imports
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
                // Inheritance
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
                _ => {}
            }
        }

        // If we have a call but no caller, walk up the AST to find the enclosing function.
        // Fall back to file path so top-level references (e.g. SQL SELECT without DDL) still produce edges.
        if rel_kind == Some(RelationKind::Calls) && source_name.is_none() {
            if let Some(site) = site_node {
                source_name =
                    find_enclosing_function(site, source).or_else(|| Some(file.to_string()));
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
                receiver: None,
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

fn node_text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}
