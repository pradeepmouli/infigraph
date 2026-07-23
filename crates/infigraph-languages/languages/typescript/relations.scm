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

; Class inheritance: class Foo extends Bar (Bar may be identifier or member_expression)
(class_declaration
  name: (type_identifier) @inherit.child
  (class_heritage
    (extends_clause
      value: (_) @inherit.parent)))

; Interface inheritance: interface Foo extends Bar (Bar may be type_identifier,
; generic_type, or nested_type_identifier)
(interface_declaration
  name: (type_identifier) @inherit.child
  (extends_type_clause
    type: (_) @inherit.parent))

; Class implements: class Foo implements Bar
(class_declaration
  name: (type_identifier) @inherit.child
  (class_heritage
    (implements_clause
      (_) @inherit.parent)))
