; Scala relationship extraction queries

; Function calls
(call_expression
  (identifier) @call.func) @call.site

; Import declarations
(import_declaration
  (identifier) @import.module)

; Extends clause (inheritance)
(extends_clause
  (type_identifier) @inherit.parent)
