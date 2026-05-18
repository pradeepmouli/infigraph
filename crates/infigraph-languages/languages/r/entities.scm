; R entity extraction queries

; Function assignments: func_name <- function(...) { ... }
(binary_operator
  lhs: (identifier) @func.name
  rhs: (function_definition)) @func.def

; Left assignment with equals: func_name = function(...) { ... }
; R uses binary_operator for all assignment forms (=, <-, <<-)
; The first binary_operator rule above already covers <- and =
