; OCaml entity extraction queries

; Let bindings (function definitions)
(let_binding
  (value_name) @func.name) @func.def

; Type definitions
(type_binding
  name: (type_constructor) @class.name) @class.def

; Module definitions
(module_definition
  (module_binding
    (module_name) @class.name)) @class.def
