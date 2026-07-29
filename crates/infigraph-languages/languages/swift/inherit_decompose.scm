; Resolves a compound @inherit.parent/@inherit.child capture (user_type wrapping
; a generic e.g. Bar<T>, or a flat qualified name e.g. pkg.Bar) down to its base
; identifier. Swift's grammar declares NO fields on user_type at all (confirmed
; empirically, structurally identical to Kotlin's user_type) -- kind + anchor
; based, same rationale as kotlin/inherit_decompose.scm.
[
  (user_type (type_identifier) @candidate . (type_arguments))
  (user_type (type_identifier) @candidate .)
]
