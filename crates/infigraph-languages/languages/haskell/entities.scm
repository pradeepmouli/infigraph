; Haskell entity extraction queries

; Function declarations
(function
  name: (variable) @func.name) @func.def

; Type declarations (data, newtype)
(data_type
  name: (name) @class.name) @class.def

; Type class declarations
(class
  name: (name) @class.name) @class.def

; Type signatures
(signature
  name: (variable) @func.name) @func.def

; === HTTP Route Patterns (Scotty, Servant) ===
(apply
  (apply
    (variable) @route.method
    (literal
      (string) @route.path))
  (#match? @route.method "^(get|post|put|delete|patch|options|middleware)$")) @route.def
