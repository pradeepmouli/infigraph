; Verilog/SystemVerilog entity extraction queries

; Module declarations
(module_declaration) @module.def

; Function declarations
(function_declaration
  (function_body_declaration
    (function_identifier) @func.name)) @func.def

; Task declarations
(task_declaration
  (task_body_declaration
    (task_identifier) @func.name)) @func.def
