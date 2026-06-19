use tree_sitter::Language;

extern "C" {
    fn tree_sitter_vb6() -> Language;
}

/// Get the tree-sitter [Language] for VB6.
pub fn language() -> Language {
    unsafe { tree_sitter_vb6() }
}

/// The content of the `node-types.json` file for this grammar.
pub const NODE_TYPES: &str = include_str!("node-types.json");

#[cfg(test)]
mod tests {
    fn make_parser() -> tree_sitter::Parser {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::language())
            .expect("Error loading VB6 grammar");
        parser
    }

    #[test]
    fn test_can_load_grammar() {
        make_parser();
    }

    #[test]
    fn test_parse_variable_declaration() {
        let mut parser = make_parser();
        let tree = parser.parse("Dim x As Integer\n", None).unwrap();
        let root = tree.root_node();
        assert!(
            !root.has_error(),
            "expected no parse errors: {}",
            root.to_sexp()
        );
        assert_eq!(root.kind(), "source_file");
    }

    #[test]
    fn test_parse_function_declaration() {
        let mut parser = make_parser();
        let tree = parser
            .parse(
                "Function Bar() As Integer\n    Bar = 42\nEnd Function\n",
                None,
            )
            .unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "expected no parse errors");
    }

    #[test]
    fn test_parse_class_module() {
        let mut parser = make_parser();
        let src = "Option Explicit\nPrivate mName As String\nPublic Property Get Name() As String\n    Name = mName\nEnd Property\n";
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "expected no parse errors");
    }

    #[test]
    fn test_parse_invalid_syntax() {
        let mut parser = make_parser();
        let tree = parser.parse("Sub\nEnd\n", None).unwrap();
        let root = tree.root_node();
        assert!(root.has_error(), "expected parse errors for invalid syntax");
    }

    #[test]
    fn test_node_types_nonempty() {
        assert!(
            !super::NODE_TYPES.is_empty(),
            "NODE_TYPES should not be empty"
        );
    }
}
