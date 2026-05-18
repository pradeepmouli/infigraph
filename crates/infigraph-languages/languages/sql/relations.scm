; SQL relationship extraction queries

; Function calls
(invocation
  (object_reference
    name: (identifier) @call.func)) @call.site

; Table ref in CREATE TABLE ... AS SELECT ... FROM
(create_table
  (object_reference name: (identifier) @call.caller)
  (create_query
    (from
      (relation
        (object_reference name: (identifier) @call.func))))) @call.site

; JOIN ref in CREATE TABLE ... AS SELECT ... JOIN
(create_table
  (object_reference name: (identifier) @call.caller)
  (create_query
    (from
      (join
        (relation
          (object_reference name: (identifier) @call.func)))))) @call.site

; Table ref in CTE body (WITH cte AS (SELECT ... FROM ...))
(cte
  (identifier) @call.caller
  (statement
    (from
      (relation
        (object_reference name: (identifier) @call.func))))) @call.site

; Table ref in INSERT INTO ... SELECT ... FROM
(insert
  (object_reference name: (identifier) @call.caller)
  (from
    (relation
      (object_reference name: (identifier) @call.func)))) @call.site

; JOIN ref in INSERT INTO ... SELECT ... JOIN
(insert
  (object_reference name: (identifier) @call.caller)
  (from
    (join
      (relation
        (object_reference name: (identifier) @call.func))))) @call.site

; Generic FROM ref (fallback — caller resolved by walking AST)
(from
  (relation
    (object_reference name: (identifier) @call.func))) @call.site

; Generic JOIN ref (fallback — caller resolved by walking AST)
(from
  (join
    (relation
      (object_reference name: (identifier) @call.func)))) @call.site
