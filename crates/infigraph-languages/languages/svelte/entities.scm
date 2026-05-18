; Svelte entity extraction queries

; Script tags
(script_element
  (start_tag
    (tag_name) @section.name)) @section.def

; Component elements (custom elements with uppercase names)
(element
  (start_tag
    (tag_name) @section.name)) @section.def
