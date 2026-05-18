; R relationship extraction queries

; Function calls
(call
  function: (identifier) @call.func) @call.site

; Library/require calls (imports)
(call
  function: (identifier) @call.func
  (#match? @call.func "^(library|require)$")
  arguments: (arguments
    (identifier) @import.module))
