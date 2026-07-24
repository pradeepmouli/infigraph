; Rust relationship extraction queries

; Function/method calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (field_expression
    value: (_) @call.receiver
    field: (field_identifier) @call.func)) @call.site

; Use declarations (imports)
(use_declaration
  argument: (scoped_identifier) @import.module)

(use_declaration
  argument: (identifier) @import.module)

; Struct inheritance via trait bounds isn't direct, but impl Trait for Type is
; We capture impl blocks as a relationship. trait/type may be type_identifier,
; generic_type, or scoped_type_identifier (e.g. impl std::fmt::Display for MyType,
; impl Iterator<Item=T> for MyType).
(impl_item
  trait: (_) @inherit.parent
  type: (_) @inherit.child)
