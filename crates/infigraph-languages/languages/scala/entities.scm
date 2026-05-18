; Scala entity extraction queries

; Class definitions
(class_definition
  name: (identifier) @class.name) @class.def

; Object definitions
(object_definition
  name: (identifier) @class.name) @class.def

; Trait definitions
(trait_definition
  name: (identifier) @class.name) @class.def

; Enum definitions
(enum_definition
  name: (identifier) @class.name) @class.def

; Function definitions
(function_definition
  name: (identifier) @func.name) @func.def

; Val definitions
(val_definition
  pattern: (identifier) @var.name) @var.def

; Var definitions
(var_definition
  pattern: (identifier) @var.name) @var.def

; Type definitions
(type_definition
  name: (type_identifier) @class.name) @class.def

; === HTTP Route Patterns (Akka HTTP, Play) ===

; path("segment") { ... } / pathPrefix("segment") { ... }
(call_expression
  function: (identifier) @route.method
  arguments: (arguments
    (string) @route.path)
  (#match? @route.method "^(path|pathPrefix|pathEnd|pathSuffix|get|post|put|delete|patch|options|head)$")) @route.def
