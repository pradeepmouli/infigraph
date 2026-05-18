; GraphQL entity extraction queries

; Object type definitions
(object_type_definition
  (name) @class.name) @class.def

; Interface type definitions
(interface_type_definition
  (name) @class.name) @class.def

; Enum type definitions
(enum_type_definition
  (name) @class.name) @class.def

; Input object type definitions
(input_object_type_definition
  (name) @class.name) @class.def

; Union type definitions
(union_type_definition
  (name) @class.name) @class.def

; === GraphQL Query/Mutation/Subscription fields as routes ===

; Field definitions inside any type (Query, Mutation, Subscription fields)
(object_type_definition
  (fields_definition
    (field_definition
      (name) @method.name) @method.def))
