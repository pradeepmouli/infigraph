; Pascal / Delphi relation extraction queries

; Function / procedure calls — direct: Foo()
(exprCall
  entity: (identifier) @call.target) @call.site

; Method calls — dotted: Obj.Method() or Unit.Proc()
; Capture rightmost identifier (the method/function name)
(exprCall
  entity: (exprDot
    rhs: (identifier) @call.target)) @call.site

; Inherited method calls: inherited Create, inherited Destroy, etc.
(inherited
  (identifier) @call.target) @call.site

; Uses clause — imports (each module name is an identifier in moduleName)
(declUses
  (moduleName
    (identifier) @import.target)) @import.site
