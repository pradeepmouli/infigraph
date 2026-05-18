; Dart entity extraction queries

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Enum declarations
(enum_declaration
  name: (identifier) @class.name) @class.def

; Mixin declarations
(mixin_declaration
  (identifier) @class.name) @class.def

; Extension declarations
(extension_declaration
  name: (identifier) @class.name) @class.def

; Top-level function declarations
(function_declaration
  signature: (function_signature
    name: (identifier) @func.name)) @func.def

; Method signatures
(method_signature
  (function_signature
    name: (identifier) @method.name)) @method.def

; Constructor signatures
(constructor_signature
  name: (identifier) @method.name) @method.def

; Top-level variables
(top_level_variable_declaration
  (static_final_declaration_list
    (static_final_declaration
      name: (identifier) @var.name))) @var.def

; === HTTP Route Patterns (shelf, dart_frog) ===
(expression_statement
  (call_expression
    function: (identifier) @route.method
    arguments: (arguments
      (string_literal) @route.path))
  (#match? @route.method "^(get|post|put|delete|patch|mount)$")) @route.def
