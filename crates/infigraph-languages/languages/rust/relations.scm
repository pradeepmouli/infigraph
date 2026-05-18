; Rust relationship extraction queries

; Function/method calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (field_expression
    field: (field_identifier) @call.func)) @call.site

; Use declarations (imports)
(use_declaration
  argument: (scoped_identifier) @import.module)

(use_declaration
  argument: (identifier) @import.module)

; Struct inheritance via trait bounds isn't direct, but impl Trait for Type is
; We capture impl blocks as a relationship
