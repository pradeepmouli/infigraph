; Swift relationship extraction queries

; Function calls
(call_expression
  (simple_identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  (navigation_expression
    (_) @call.receiver
    (navigation_suffix
      (simple_identifier) @call.func))) @call.site

; Import declarations
(import_declaration
  (identifier) @import.module)

; Class/struct/enum/protocol inheritance or protocol conformance:
; class Foo: Bar, protocol Foo: Bar (Bar may be generic e.g. Comparable<Foo>,
; or qualified e.g. pkg.Bar)
(class_declaration
  name: (type_identifier) @inherit.child
  (inheritance_specifier
    inherits_from: (_) @inherit.parent))

(protocol_declaration
  name: (type_identifier) @inherit.child
  (inheritance_specifier
    inherits_from: (_) @inherit.parent))
