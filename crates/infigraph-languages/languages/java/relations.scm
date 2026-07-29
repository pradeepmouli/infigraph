; Java relationship extraction queries

; Method invocations on objects: obj.method()
(method_invocation
  object: (_) @call.receiver
  name: (identifier) @call.func) @call.site

; Unqualified method invocations: method()
(method_invocation
  !object
  name: (identifier) @call.func) @call.site

; Object creation: new Foo()
(object_creation_expression
  type: (type_identifier) @call.func) @call.site

; Import declarations
(import_declaration
  (scoped_identifier) @import.module)

; Class inheritance: extends. May be type_identifier, generic_type, or
; scoped_type_identifier (e.g. class Foo extends Bar<T>, class Foo extends pkg.Bar).
(class_declaration
  name: (identifier) @inherit.child
  (superclass
    (_) @inherit.parent))

; Interface implementation: implements
(class_declaration
  name: (identifier) @inherit.child
  (super_interfaces
    (type_list
      (_) @inherit.parent)))
