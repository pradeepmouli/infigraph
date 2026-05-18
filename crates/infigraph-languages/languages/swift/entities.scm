; Swift entity extraction queries

; Function declarations
(function_declaration
  name: (simple_identifier) @func.name) @func.def

; Class/struct/enum/extension declarations (all use class_declaration)
(class_declaration
  name: (type_identifier) @class.name) @class.def

; Protocol declarations
(protocol_declaration
  name: (type_identifier) @class.name) @class.def

; Property declarations
(property_declaration
  (pattern
    (simple_identifier) @var.name)) @var.def

; === HTTP Route Patterns (Vapor) ===
(call_expression
  (navigation_expression
    (navigation_suffix
      (simple_identifier) @route.method))
  (call_suffix
    (value_arguments
      (value_argument
        (line_string_literal) @route.path)))
  (#match? @route.method "^(get|post|put|delete|patch)$")) @route.def
