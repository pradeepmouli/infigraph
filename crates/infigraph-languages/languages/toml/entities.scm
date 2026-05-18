; TOML entity extraction queries

; Table headers (sections)
(table
  (bare_key) @section.name) @section.def

; Table array headers
(table_array_element
  (bare_key) @section.name) @section.def

; Top-level key-value pairs
(document
  (pair
    (bare_key) @var.name) @var.def)
