; CMake entity extraction queries

; Function definitions
(function_def
  (function_command
    (argument_list
      (argument) @func.name))) @func.def

; Macro definitions
(macro_def
  (macro_command
    (argument_list
      (argument) @func.name))) @func.def
