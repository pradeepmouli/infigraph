; VB6 relation extraction queries

; Direct Sub/Function call: Call Foo arg1, arg2
(call_statement
  name: (identifier) @call.func) @call.site

; Function call — direct or dotted: Foo(), Obj.Method()
; qualified_identifier contains one or more identifier children
; All are captured; the last identifier resolves to the method name, earlier ones are noise
(function_call
  name: (qualified_identifier
    (identifier) @call.func)) @call.site
