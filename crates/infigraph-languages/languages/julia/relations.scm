; Julia relationship extraction queries

; Function calls
(call_expression
  (identifier) @call.func) @call.site

; Import statements
(import_statement
  (identifier) @import.module)

; Using statements
(using_statement
  (identifier) @import.module)
