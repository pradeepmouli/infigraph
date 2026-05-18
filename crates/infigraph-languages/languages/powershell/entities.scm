; PowerShell entity extraction queries

; Function definitions
(function_statement
  (function_name) @func.name) @func.def

; Class definitions
(class_statement
  (simple_name) @class.name) @class.def

; Class method definitions
(class_statement
  (class_method_definition
    (simple_name) @method.name) @method.def)
