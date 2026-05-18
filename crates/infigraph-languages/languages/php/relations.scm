; PHP relationship extraction queries

; Function calls
(function_call_expression
  function: (name) @call.func) @call.site

; Scoped calls: Class::method()
(scoped_call_expression
  name: (name) @call.func) @call.site

; Member calls: $obj->method()
(member_call_expression
  name: (name) @call.func) @call.site

; Object creation: new Foo()
(object_creation_expression
  (name) @call.func) @call.site

; Class inheritance: extends
(class_declaration
  name: (name) @inherit.child
  (base_clause
    (name) @inherit.parent))

; Interface implementation: implements
(class_interface_clause
  (name) @inherit.parent)
