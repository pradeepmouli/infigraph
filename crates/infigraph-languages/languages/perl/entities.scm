; Perl entity extraction queries

; Subroutine definitions
(function_definition
  name: (identifier) @func.name) @func.def

; Package declarations (like classes)
(package_statement
  (package_name) @class.name) @class.def

; === HTTP Route Patterns (Dancer, Mojolicious) ===
(function_definition
  name: (identifier) @route.method
  body: (block
    (string_single_quoted) @route.path)
  (#match? @route.method "^(get|post|put|del|patch|any|options)$")) @route.def
