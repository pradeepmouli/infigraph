; Resolves a compound @inherit.parent/@inherit.child capture (user_type wrapping
; a generic e.g. Animal<T>, or a flat qualified name e.g. pkg.Animal) down to its
; base identifier. Kotlin's grammar declares NO fields on user_type at all
; (confirmed empirically) -- this uses kind + anchor operators rather than field
; names. `.` after the second alternative anchors to "this identifier is the
; last named child" (correctly picks the last segment of pkg.Animal); the first
; alternative anchors to "immediately followed by type_arguments" (correctly
; picks the base name before generic args, ignoring the args themselves).
[
  (user_type (identifier) @candidate . (type_arguments))
  (user_type (identifier) @candidate .)
]
