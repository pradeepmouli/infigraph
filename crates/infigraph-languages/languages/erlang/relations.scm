; Erlang relationship extraction queries

; Function calls (local)
(call
  expr: (atom) @call.func) @call.site

; Remote calls (module:function)
(call
  expr: (remote
    fun: (atom) @call.func)) @call.site
