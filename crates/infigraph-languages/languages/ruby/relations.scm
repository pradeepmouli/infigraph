; Ruby relationship extraction queries

; Method calls
(call
  method: (identifier) @call.func) @call.site

; Class inheritance: class Foo < Bar
(class
  name: (constant) @inherit.child
  (superclass
    (constant) @inherit.parent))
