; HTML entity extraction queries

; Top-level elements
(element
  (start_tag
    (tag_name) @section.name)) @section.def

; Self-closing tags
(element
  (self_closing_tag
    (tag_name) @section.name)) @section.def
