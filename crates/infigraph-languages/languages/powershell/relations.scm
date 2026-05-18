; PowerShell relationship extraction queries

; Command invocations
(command
  command_name: (command_name) @call.func) @call.site

; Method calls (invocation expressions with member access)
(invokation_expression
  (member_access
    (member_name) @call.func)) @call.site
