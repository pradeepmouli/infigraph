; C++ entity extraction queries

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

; Union definitions
(union_specifier
  name: (type_identifier) @class.name
  body: (_)) @class.def

; Enum definitions
(enum_specifier
  name: (type_identifier) @class.name) @class.def

; Typedef declarations
(type_definition
  declarator: (type_identifier) @class.name) @class.def

; Namespace definitions
(namespace_definition
  name: (namespace_identifier) @class.name) @class.def
