; Dart relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.method()
(call_expression
  function: (member_expression
    property: (identifier) @call.func)) @call.site

; Import directives
(import_specification
  uri: (uri) @import.module)
