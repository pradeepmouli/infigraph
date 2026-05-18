; OCaml relationship extraction queries

; Function application
(application_expression
  function: (value_path
    (value_name) @call.func)) @call.site

; Open statements (imports)
(open_module
  (module_path
    (module_name) @import.module))
