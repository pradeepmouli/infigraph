; JavaScript relationship extraction queries

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

; Require calls
(call_expression
  function: (identifier) @_req
  (#eq? @_req "require")
  arguments: (arguments
    (string) @import.module))

; Class inheritance: extends
(class_declaration
  name: (identifier) @inherit.child
  (class_heritage
    (identifier) @inherit.parent))
