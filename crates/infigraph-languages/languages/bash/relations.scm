; Bash/Shell relationship extraction queries

; Command invocations
(command
  name: (command_name) @call.func) @call.site

; Source/dot imports
(command
  name: (command_name) @call.func
  (#match? @call.func "^(source|\\.)$")
  argument: (_) @import.module)
