; Swift relationship extraction queries

; Function calls
(call_expression
  (simple_identifier) @call.func) @call.site

; Import declarations
(import_declaration
  (identifier) @import.module)
