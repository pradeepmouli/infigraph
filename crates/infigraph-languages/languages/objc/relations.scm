; Objective-C relationship extraction queries

; Function calls (C-style)
(call_expression
  function: (identifier) @call.func) @call.site

; Import/include (#import and #include both use preproc_include)
(preproc_include
  path: (_) @import.module)

; Module import (@import)
(module_import
  path: (identifier) @import.module)
