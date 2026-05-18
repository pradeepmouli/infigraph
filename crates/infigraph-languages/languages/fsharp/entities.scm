; F# entity extraction queries

; Module definitions
(module_defn
  . (identifier) @section.name) @section.def

; Type definitions (record, union, interface, enum, etc.)
(type_definition
  [
    (record_type_defn
      (type_name
        type_name: (_) @class.name))
    (union_type_defn
      (type_name
        type_name: (_) @class.name))
    (interface_type_defn
      (type_name
        type_name: (_) @class.name))
    (enum_type_defn
      (type_name
        type_name: (_) @class.name))
    (type_abbrev_defn
      (type_name
        type_name: (_) @class.name))
  ]) @class.def

; Function definitions
(function_or_value_defn
  (function_declaration_left
    . (_) @func.name)) @func.def

; Member definitions (methods/properties)
(member_defn
  (method_or_prop_defn
    name: (identifier) @method.name)) @method.def

; F# route patterns (Giraffe/Saturn) handled by AST-based attribute scan in entities.rs
