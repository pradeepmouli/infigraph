; CUDA entity extraction queries

; Function definitions
(function_declarator
  declarator: (identifier) @func.name) @func.def

; Method definitions (qualified)
(function_declarator
  declarator: (field_identifier) @method.name) @method.def

; Class definitions
(class_specifier
  name: (type_identifier) @class.name) @class.def

; Struct definitions
(struct_specifier
  name: (type_identifier) @class.name
  body: (_)) @class.def
