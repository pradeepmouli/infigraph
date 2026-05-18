; Python relationship extraction queries for infigraph

; Function/method calls
(call
  function: (identifier) @call.func) @call.site

; Method calls on objects (obj.method())
(call
  function: (attribute
    attribute: (identifier) @call.func)) @call.site

; Import statements: import foo
(import_statement
  name: (dotted_name) @import.module)

; From imports: from foo import bar
(import_from_statement
  module_name: (dotted_name) @import.module)

; Class inheritance: class Foo(Bar)
(class_definition
  name: (identifier) @inherit.child
  superclasses: (argument_list
    (identifier) @inherit.parent))
