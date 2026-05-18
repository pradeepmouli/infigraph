; Objective-C entity extraction queries

; Function definitions (C-style)
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @func.name)) @func.def

; Method declarations
(method_definition
  (identifier) @method.name) @method.def

; Class interface declarations
(class_interface
  name: (identifier) @class.name) @class.def

; Class implementation
(class_implementation
  name: (identifier) @class.name) @class.def

; Protocol declarations
(protocol_declaration
  name: (identifier) @class.name) @class.def

; Category interface (ObjC categories use class_interface with category field)
; Captured by the class_interface rule above
