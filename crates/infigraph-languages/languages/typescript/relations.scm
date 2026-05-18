; TypeScript relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (member_expression
    property: (property_identifier) @call.func)) @call.site

; Import statements
(import_statement
  source: (string) @import.module)
