; Bash/Shell entity extraction queries

; Function definitions
(function_definition
  name: (word) @func.name) @func.def

; Variable assignments
(variable_assignment
  name: (variable_name) @var.name) @var.def
