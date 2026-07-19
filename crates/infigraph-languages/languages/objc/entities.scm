; Objective-C entity extraction queries

; Function definitions (C-style)
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @func.name)) @func.def

; Method declarations
(method_definition
  (identifier) @method.name) @method.def

; Class interface declarations
; class_interface has no "name" field (its grammar rule injects the
; identifier positionally, right after @interface) -- name: (identifier)
; never matched anything, so no class symbols were ever extracted.
(class_interface
  (identifier) @class.name) @class.def

; Class implementation (same field-less structure as class_interface)
(class_implementation
  (identifier) @class.name) @class.def

; Protocol declarations (same field-less structure)
(protocol_declaration
  (identifier) @class.name) @class.def

; Category interface (ObjC categories use class_interface with category field)
; Captured by the class_interface rule above
