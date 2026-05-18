; Kotlin relationship extraction queries

; Simple function calls: funcName()
(call_expression
  (identifier) @call.func) @call.site

; Import declarations
(import
  (identifier) @import.module)
