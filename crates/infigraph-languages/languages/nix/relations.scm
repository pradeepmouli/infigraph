; Nix relationship extraction queries

; Function application (calls)
(apply_expression
  function: (variable_expression
    name: (identifier) @call.func)) @call.site
