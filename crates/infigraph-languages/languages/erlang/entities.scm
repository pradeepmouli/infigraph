; Erlang entity extraction queries

; Module attribute
(module_attribute
  name: (atom) @module.name) @module.def

; Function declarations (first clause defines the function)
(function_clause
  name: (atom) @func.name) @func.def

; === HTTP Route Patterns (Cowboy) ===
(call
  expr: (atom) @route.method
  args: (expr_args
    (string) @route.path)
  (#match? @route.method "^(dispatch_rules|init|handle)$")) @route.def
