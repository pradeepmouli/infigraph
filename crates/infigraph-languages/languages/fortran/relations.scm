; Fortran relationship extraction queries

; Function/subroutine calls
(call_expression
  function: (identifier) @call.func) @call.site

; Use statements (imports)
(use_statement
  (module_name) @import.module)
