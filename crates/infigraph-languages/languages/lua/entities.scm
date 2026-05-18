; Lua entity extraction queries

; Function declarations (named)
(function_declaration
  name: (identifier) @func.name) @func.def

; Function declarations (dot index, e.g. M.func)
(function_declaration
  name: (dot_index_expression
    field: (identifier) @func.name)) @func.def

; Method declarations (colon syntax, e.g. M:method)
(function_declaration
  name: (method_index_expression
    method: (identifier) @method.name)) @method.def

; Variable assigned to function
(assignment_statement
  (variable_list
    name: (identifier) @func.name)
  (expression_list
    value: (function_definition))) @func.def

; === HTTP Route Patterns (Lapis, OpenResty) ===
(function_call
  name: (identifier) @route.method
  arguments: (arguments
    (string) @route.path)
  (#match? @route.method "^(get|post|put|delete|match)$")) @route.def
