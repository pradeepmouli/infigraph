; Rust entity extraction queries

; Function definitions
(function_item
  name: (identifier) @func.name) @func.def

; Struct definitions
(struct_item
  name: (type_identifier) @class.name) @class.def

; Enum definitions
(enum_item
  name: (type_identifier) @class.name) @class.def

; Trait definitions
(trait_item
  name: (type_identifier) @class.name) @class.def

; Impl block methods
(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @method.name) @method.def))

; Const items
(const_item
  name: (identifier) @var.name) @var.def

; Static items
(static_item
  name: (identifier) @var.name) @var.def

; Type aliases
(type_item
  name: (type_identifier) @class.name) @class.def
