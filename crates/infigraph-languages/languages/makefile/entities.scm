; Makefile entity extraction queries

; Make rules (targets)
(rule
  (targets) @section.name) @section.def

; Variable assignments
(variable_assignment
  name: (word) @var.name) @var.def
