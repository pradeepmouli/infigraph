; Fortran entity extraction queries

; Function definitions
(function
  (function_statement
    name: (name) @func.name)) @func.def

; Subroutine definitions
(subroutine
  (subroutine_statement
    name: (name) @func.name)) @func.def

; Module definitions (module_statement has name as child, not field)
(module
  (module_statement
    (name) @module.name)) @module.def

; Program definitions (program_statement has name as child, not field)
(program
  (program_statement
    (name) @module.name)) @module.def
