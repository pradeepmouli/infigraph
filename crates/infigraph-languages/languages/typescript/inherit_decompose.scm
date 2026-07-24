; Resolves a compound @inherit.parent/@inherit.child capture (generic_type,
; nested_type_identifier, member_expression) down to its base identifier.
; Field-based: TypeScript's grammar names the base-identifier field "name",
; "type", or "property" depending on the wrapper node kind.
[
  (_ name: (_) @candidate)
  (_ type: (_) @candidate)
  (_ property: (_) @candidate)
]
