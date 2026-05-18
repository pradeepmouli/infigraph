; C++ relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method() or obj->method()
(call_expression
  function: (field_expression
    field: (field_identifier) @call.func)) @call.site

; Qualified calls: ns::func()
(call_expression
  function: (qualified_identifier
    name: (identifier) @call.func)) @call.site

; Include directives
(preproc_include
  path: (_) @import.module)

; Base class specifier (inheritance)
(base_class_clause
  (type_identifier) @inherit.parent)
