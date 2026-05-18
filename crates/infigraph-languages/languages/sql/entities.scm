; SQL entity extraction queries

; CREATE TABLE
(create_table
  (object_reference
    name: (identifier) @class.name)) @class.def

; CREATE FUNCTION
(create_function
  (object_reference
    name: (identifier) @func.name)) @func.def

; CREATE VIEW
(create_view
  (identifier) @class.name) @class.def

; CREATE INDEX
(create_index
  (object_reference
    name: (identifier) @class.name)) @class.def

; CTE (WITH ... AS)
(cte
  (identifier) @func.name) @func.def
