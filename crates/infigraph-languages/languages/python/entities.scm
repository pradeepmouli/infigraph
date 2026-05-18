; Python entity extraction queries for infigraph

; Top-level function definitions (undecorated)
(module
  (function_definition
    name: (identifier) @func.name
    body: (block
      (expression_statement
        (string) @func.docstring)?)) @func.def)

; Top-level decorated function definitions (Flask routes, FastAPI, etc.)
(module
  (decorated_definition
    (decorator) @func.decorator
    definition: (function_definition
      name: (identifier) @func.name
      body: (block
        (expression_statement
          (string) @func.docstring)?)) @func.def))

; Class definitions (undecorated)
(class_definition
  name: (identifier) @class.name
  body: (block
    (expression_statement
      (string) @class.docstring)?)) @class.def

; Decorated class definitions (@RestController, @Blueprint, etc.)
(module
  (decorated_definition
    (decorator) @class.decorator
    definition: (class_definition
      name: (identifier) @class.name
      body: (block
        (expression_statement
          (string) @class.docstring)?)) @class.def))

; Method definitions inside classes (undecorated)
(class_definition
  body: (block
    (function_definition
      name: (identifier) @method.name
      body: (block
        (expression_statement
          (string) @method.docstring)?)) @method.def))

; Decorated methods inside classes
(class_definition
  body: (block
    (decorated_definition
      (decorator) @method.decorator
      definition: (function_definition
        name: (identifier) @method.name
        body: (block
          (expression_statement
            (string) @method.docstring)?)) @method.def)))

; Test functions (pytest convention: functions starting with test_)
(module
  (function_definition
    name: (identifier) @test.name
    (#match? @test.name "^test_")
    body: (block
      (expression_statement
        (string) @test.docstring)?)) @test.def)

; Module-level assignments (variables/constants)
(module
  (expression_statement
    (assignment
      left: (identifier) @var.name)) @var.def)

; === HTTP Route Patterns (Django) ===

; path("route/", view_func) in urlpatterns
(call
  function: (identifier) @route.method
  arguments: (argument_list
    (string) @route.path)
  (#match? @route.method "^(path|re_path|url)$")) @route.def
