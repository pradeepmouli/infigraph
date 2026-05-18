; Markdown entity extraction queries

; Headings as sections
(atx_heading
  (inline) @section.name) @section.def

; Fenced code blocks
(fenced_code_block
  (info_string) @section.name) @section.def
