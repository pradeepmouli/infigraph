use tree_sitter::Node;

/// Cyclomatic complexity = 1 + number of decision points in the AST subtree.
///
/// Counts: if, else_if, for, while, do_while, loop, match/switch arms (case),
/// conditional expressions (?), logical AND (&&), logical OR (||), catch/except,
/// ternary (?:). Language-agnostic — matches node type strings from tree-sitter.
pub fn cyclomatic_complexity(node: Node) -> u32 {
    let mut count = 1u32; // base complexity
    count_branches(node, &mut count);
    count
}

fn count_branches(node: Node, count: &mut u32) {
    let kind = node.kind();
    match kind {
        // Conditionals
        "if_expression" | "if_statement" | "elif_clause" | "else_if_clause" |
        "else_clause" | "when_clause" |
        // Loops
        "for_statement" | "for_expression" | "for_in_statement" |
        "while_statement" | "while_expression" |
        "do_statement" | "loop_expression" |
        // Pattern matching
        "match_arm" | "case_clause" | "switch_case" | "arm" |
        // Exception handling
        "catch_clause" | "except_clause" | "rescue_clause" |
        // Ternary / conditional expression
        "ternary_expression" | "conditional_expression" |
        // Logical short-circuit operators
        "binary_expression" => {
            // For binary_expression, only count && and ||
            if kind == "binary_expression" {
                let op = node.child_by_field_name("operator")
                    .map(|n| n.kind())
                    .unwrap_or("");
                if op == "&&" || op == "||" || op == "and" || op == "or" || op == "??" {
                    *count += 1;
                }
            } else {
                *count += 1;
            }
        }
        // Null coalescing / optional chaining count as branches
        "try_expression" | "propagation_expression" => *count += 1,
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            count_branches(child, count);
        }
    }
}
