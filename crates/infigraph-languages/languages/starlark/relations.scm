; Starlark relationship extraction queries

; Function calls
(call
  function: (identifier) @call.func) @call.site

; load() imports
(call
  function: (identifier) @_fn
  (#eq? @_fn "load")
  arguments: (argument_list
    (string) @import.module))
