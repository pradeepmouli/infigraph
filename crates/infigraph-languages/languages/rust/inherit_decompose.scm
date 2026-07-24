; Resolves a compound @inherit.parent/@inherit.child capture (generic_type,
; scoped_type_identifier) down to its base identifier. Field-based: Rust's grammar
; names the base-identifier field "name" (scoped_type_identifier) or "type"
; (generic_type) depending on the wrapper node kind.
[
  (_ name: (_) @candidate)
  (_ type: (_) @candidate)
]
