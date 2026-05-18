; C relationship extraction queries

; Function calls
(call_expression
  function: (identifier) @call.func) @call.site

; Preproc include
(preproc_include
  path: (_) @import.module)
