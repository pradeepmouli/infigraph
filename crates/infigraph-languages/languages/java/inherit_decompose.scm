; Resolves a compound @inherit.parent/@inherit.child capture (generic_type,
; scoped_type_identifier) down to its base identifier. Java's grammar declares NO
; fields on either node (confirmed empirically) -- these are positional, unnamed
; children, so this uses kind + anchor operators rather than field names, unlike
; TypeScript/Rust/Go. `.` anchors to "immediately followed by nothing else in this
; alternative's remaining pattern", picking the last matching-kind child.
[
  (generic_type (type_identifier) @candidate . (type_arguments))
  (generic_type (scoped_type_identifier) @candidate . (type_arguments))
  (scoped_type_identifier (type_identifier) @candidate .)
]
