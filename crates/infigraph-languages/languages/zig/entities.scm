; Zig entity extraction queries

; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def

; Test declarations
(test_declaration
  (string) @test.name) @test.def

; Variable declarations (const/var)
(variable_declaration
  (identifier) @var.name) @var.def
