; Groovy entity extraction queries

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Interface declarations
(interface_declaration
  name: (identifier) @class.name) @class.def

; Method declarations
(method_declaration
  name: (identifier) @func.name) @func.def

; Function definitions
(function_definition
  name: (identifier) @func.name) @func.def
