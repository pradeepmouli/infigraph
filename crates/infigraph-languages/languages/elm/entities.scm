; Elm entity extraction queries

; Value/function declarations
(value_declaration
  (function_declaration_left
    (lower_case_identifier) @func.name)) @func.def

; Type declarations
(type_declaration
  name: (upper_case_identifier) @class.name) @class.def

; Type alias declarations
(type_alias_declaration
  name: (upper_case_identifier) @class.name) @class.def

; Module declaration
(module_declaration
  (upper_case_qid
    (upper_case_identifier) @section.name)) @section.def
