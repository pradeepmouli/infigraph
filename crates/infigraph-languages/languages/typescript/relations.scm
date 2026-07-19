; TypeScript relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (member_expression
    object: (_) @call.receiver
    property: (property_identifier) @call.func)) @call.site

; Import statements
(import_statement
  source: (string) @import.module)

; Class inheritance: class Foo extends Bar
(class_declaration
  name: (type_identifier) @inherit.child
  (class_heritage
    (extends_clause
      value: (identifier) @inherit.parent)))

; Interface inheritance: interface Foo extends Bar
(interface_declaration
  name: (type_identifier) @inherit.child
  (extends_type_clause
    type: (type_identifier) @inherit.parent))

; Class implements: class Foo implements Bar
(class_declaration
  name: (type_identifier) @inherit.child
  (class_heritage
    (implements_clause
      (type_identifier) @inherit.parent)))
