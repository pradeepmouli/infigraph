; Resolves a compound @inherit.parent/@inherit.child capture (attribute for dotted
; names like pkg.Animal, subscript for Generic[T]-style base classes) down to its
; base identifier. Field-based: Python's grammar names these "attribute" and "value"
; respectively -- neither matches the "name"/"type"/"property" convention used by
; TypeScript/Rust/Go, confirmed empirically against the real grammar.
[
  (attribute attribute: (_) @candidate)
  (subscript value: (_) @candidate)
]
