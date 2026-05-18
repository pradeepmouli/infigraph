; C# relationship extraction queries

; Method invocations
(invocation_expression
  function: (member_access_expression
    name: (identifier) @call.func)) @call.site

; Simple invocations
(invocation_expression
  function: (identifier) @call.func) @call.site

; Object creation: new Foo()
(object_creation_expression
  type: (identifier) @call.func) @call.site

; Using directives
(using_directive
  (identifier) @import.module)

; Using directives (qualified name)
(using_directive
  (qualified_name) @import.module)

; Class inheritance: base list
(class_declaration
  name: (identifier) @inherit.child
  (base_list
    (identifier) @inherit.parent))

; Interface inheritance
(interface_declaration
  name: (identifier) @inherit.child
  (base_list
    (identifier) @inherit.parent))
