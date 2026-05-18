; Go entity extraction queries

; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def

; Method declarations (with receiver)
(method_declaration
  name: (field_identifier) @method.name) @method.def

; Type declarations - struct
(type_declaration
  (type_spec
    name: (type_identifier) @class.name
    type: (struct_type))) @class.def

; Type declarations - interface
(type_declaration
  (type_spec
    name: (type_identifier) @class.name
    type: (interface_type))) @class.def

; Const declarations
(const_declaration
  (const_spec
    name: (identifier) @var.name)) @var.def

; Var declarations
(var_declaration
  (var_spec
    name: (identifier) @var.name)) @var.def

; Test functions (Go convention: func TestXxx)
(function_declaration
  name: (identifier) @test.name
  (#match? @test.name "^Test")) @test.def

; === HTTP Route Patterns (Gin, Echo, Fiber, Chi, Gorilla Mux, net/http) ===

; r.GET("/path", handler) — with named handler
(expression_statement
  (call_expression
    function: (selector_expression
      field: (field_identifier) @route.method)
    arguments: (argument_list
      (interpreted_string_literal) @route.path
      (identifier) @route.handler)) @route.def
  (#match? @route.method "^(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Get|Post|Put|Delete|Patch|Options|Head|HandleFunc|Handle|Any|Group)$"))

; r.GET("/path", ...) — without named handler
(expression_statement
  (call_expression
    function: (selector_expression
      field: (field_identifier) @route.method)
    arguments: (argument_list
      (interpreted_string_literal) @route.path)) @route.def
  (#match? @route.method "^(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Get|Post|Put|Delete|Patch|Options|Head|HandleFunc|Handle|Any|Group)$"))

; http.HandleFunc("/path", handler) — package-level call
(expression_statement
  (call_expression
    function: (selector_expression
      operand: (identifier) @_pkg
      field: (field_identifier) @route.method)
    arguments: (argument_list
      (interpreted_string_literal) @route.path)) @route.def
  (#match? @route.method "^(HandleFunc|Handle|ListenAndServe)$"))
