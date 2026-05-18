; Elixir relationship extraction queries

; Local function calls
(call
  target: (identifier) @call.func) @call.site

; Remote function calls: Module.func()
(call
  target: (dot
    right: (identifier) @call.func)) @call.site

; Pipe into function
(binary_operator
  operator: "|>"
  right: (identifier) @call.func) @call.site

; Module references
(alias) @import.module
