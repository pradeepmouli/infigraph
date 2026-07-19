; Dart relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (member_expression
    object: (_) @call.receiver
    property: (identifier) @call.func)) @call.site

; Import directives
(import_specification
  uri: (uri) @import.module)

; Class extends: class Dog extends Animal
(class_declaration
  name: (identifier) @inherit.child
  superclass: (superclass
    (type
      (type_identifier) @inherit.parent)))

; Class implements: class Foo implements Bar
(class_declaration
  name: (identifier) @inherit.child
  interfaces: (interfaces
    (type
      (type_identifier) @inherit.parent)))
