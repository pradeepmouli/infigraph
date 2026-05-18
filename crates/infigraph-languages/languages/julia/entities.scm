; Julia entity extraction queries

; Function definitions
(function_definition
  (signature
    (call_expression
      (identifier) @func.name))) @func.def

; Short function definitions (f(x) = ...) are assignments
(assignment
  (call_expression
    (identifier) @func.name)) @func.def

; Struct definitions
(struct_definition
  (type_head
    (identifier) @class.name)) @class.def

; Abstract type definitions
(abstract_definition
  (type_head
    (identifier) @class.name)) @class.def

; Module definitions
(module_definition
  name: (identifier) @class.name) @class.def

; Macro definitions
(macro_definition
  (signature
    (call_expression
      (identifier) @func.name))) @func.def
