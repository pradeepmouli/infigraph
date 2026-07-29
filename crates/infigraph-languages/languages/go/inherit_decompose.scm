; Resolves a compound @inherit.parent/@inherit.child capture (generic_type,
; qualified_type) down to its base identifier. Field-based: Go's grammar names
; the base-identifier field "name" (qualified_type) or "type" (generic_type)
; depending on the wrapper node kind.
[
  (_ name: (_) @candidate)
  (_ type: (_) @candidate)
]
