; GLSL entity extraction queries

; Function definitions (C-like)
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @func.name)) @func.def

; Struct definitions
(struct_specifier
  name: (type_identifier) @class.name) @class.def
