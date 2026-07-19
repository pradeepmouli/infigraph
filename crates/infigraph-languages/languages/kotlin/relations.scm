; Kotlin relationship extraction queries

; Simple function calls: funcName()
(call_expression
  (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  (navigation_expression
    (_) @call.receiver
    (identifier) @call.func)) @call.site

; Import declarations
(import
  (identifier) @import.module)

; Class inheritance / interface implementation: class Dog : Animal() or class Foo : Bar
(class_declaration
  name: (identifier) @inherit.child
  (delegation_specifiers
    (delegation_specifier
      [
        (user_type (identifier) @inherit.parent)
        (constructor_invocation (user_type (identifier) @inherit.parent))
      ])))
