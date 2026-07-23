; Go relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Method calls: obj.Method()
(call_expression
  function: (selector_expression
    operand: (_) @call.receiver
    field: (field_identifier) @call.func)) @call.site

; Package calls: pkg.Func()
(call_expression
  function: (selector_expression
    operand: (identifier) @_pkg
    field: (field_identifier) @call.func)) @call.site

; Import declarations
(import_spec
  path: (interpreted_string_literal) @import.module)

; Goroutine spawns: go someFunc()
(go_statement
  (call_expression
    function: (identifier) @spawns.target)) @spawns.site

; Goroutine spawns with method: go obj.Method()
(go_statement
  (call_expression
    function: (selector_expression
      field: (field_identifier) @spawns.target))) @spawns.site

; Struct embedding: type Dog struct { Animal } -- Go's closest analog to
; inheritance. An embedded (anonymous) field has no name, only a type (which
; may be type_identifier, generic_type, or qualified_type, e.g. embedding
; pkg.Animal or a generic Base[T]). Interface satisfaction is implicit/
; structural in Go and can't be determined from syntax alone, so it isn't
; captured here.
(type_spec
  name: (type_identifier) @inherit.child
  type: (struct_type
    (field_declaration_list
      (field_declaration
        !name
        type: (_) @inherit.parent))))
