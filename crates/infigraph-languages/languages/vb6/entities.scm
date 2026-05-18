; VB6 entity extraction queries

; Module name from VB_Name attribute — present in .cls and .frm files
; Attribute VB_Name = "MyClass"
(attribute_statement
  (identifier) @_attr_name
  (attribute_value
    (string) @module.name)
  (#eq? @_attr_name "VB_Name")) @module.def

; Sub definitions
(sub_definition
  name: (identifier) @func.name) @func.def

; Function definitions
(function_definition
  name: (identifier) @func.name) @func.def

; Property definitions (Get / Let / Set — all share property_definition node)
(property_definition
  name: (identifier) @func.name) @func.def

; Module-level variable declarations (Dim / Public / Private / Global / Const x As Type)
(variable_definition
  name: (identifier) @var.name) @var.def
