; Kotlin entity extraction queries (tree-sitter-kotlin-ng grammar)

; Function declarations
; function_value_parameters and the return type are positional children
; on this grammar, not named fields -- extract_child_text can't reach
; either via child_by_field_name, so capture them explicitly here.
(function_declaration
  name: (identifier) @func.name
  (function_value_parameters) @func.params
  (type)? @func.return_type) @func.def

; Annotated function declarations, bare (@GetMapping)
(function_declaration
  (modifiers
    (annotation
      (user_type
        (identifier) @func.decorator)))
  name: (identifier) @func.name) @func.def

; Annotated function declarations, with arguments (@PostMapping("/x"))
(function_declaration
  (modifiers
    (annotation
      (constructor_invocation
        (user_type
          (identifier) @func.decorator)
        (value_arguments
          (value_argument
            (string_literal
              (string_content) @func.docstring))?))))
  name: (identifier) @func.name) @func.def

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Annotated class declarations, bare (@RestController)
(class_declaration
  (modifiers
    (annotation
      (user_type
        (identifier) @class.decorator)))
  name: (identifier) @class.name) @class.def

; Annotated class declarations, with arguments (@RequestMapping("/api"))
(class_declaration
  (modifiers
    (annotation
      (constructor_invocation
        (user_type
          (identifier) @class.decorator)
        (value_arguments
          (value_argument
            (string_literal
              (string_content) @class.docstring))?))))
  name: (identifier) @class.name) @class.def

; Object declarations
(object_declaration
  name: (identifier) @class.name) @class.def

; === HTTP Route Patterns (Ktor) ===

; get("/path") { ... } / post("/path") { ... }
(call_expression
  (identifier) @route.method
  (value_arguments
    (value_argument
      (string_literal
        (string_content) @route.path)))
  (#match? @route.method "^(get|post|put|delete|patch|head|options|route|authenticate)$")) @route.def
