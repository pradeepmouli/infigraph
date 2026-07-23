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

; Class extends: class Dog extends Animal (Animal may be generic e.g.
; Animal<T>, or qualified e.g. pkg.Animal). Dart's grammar produces a second
; sibling `type`-field node for the generic type-argument list itself when
; present -- the leading anchor restricts to the FIRST type-field child (the
; base type), and the trailing anchor requires type_identifier to be that
; node's only child, which the generic-args blob's own nested `type` wrapper
; never satisfies. No separate decomposition query needed; this single
; fully-anchored pattern handles all three shapes.
(class_declaration
  name: (identifier) @inherit.child
  superclass: (superclass . (type (type_identifier) @inherit.parent .)))

; Class implements: class Foo implements Bar (each listed interface produces
; its own edge; same anchor reasoning as extends above).
(class_declaration
  name: (identifier) @inherit.child
  interfaces: (interfaces (type . (type_identifier) @inherit.parent .)))
