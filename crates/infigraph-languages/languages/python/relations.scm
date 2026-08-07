; Python relationship extraction queries for terragraph

; Function/method calls
(call
  function: (identifier) @call.func) @call.site

; Method calls on objects (obj.method())
(call
  function: (attribute
    object: (_) @call.receiver
    attribute: (identifier) @call.func)) @call.site

; Import statements: import foo
(import_statement
  name: (dotted_name) @import.module)

; From imports: from foo import bar
(import_from_statement
  module_name: (dotted_name) @import.module)

; Relative from-imports: from .foo import bar / from ..pkg.foo import bar.
; module_name is (relative_import (import_prefix) (dotted_name)?) here, not a
; bare dotted_name, so the pattern above never matches these on its own.
(import_from_statement
  module_name: (relative_import
    (dotted_name) @import.module))

; Class inheritance: class Foo(Bar). superclasses can be plain identifiers, dotted
; names (pkg.Bar), or subscripted generics (Generic[T]); matching the "expression"
; supertype (rather than a bare wildcard) correctly excludes keyword_argument nodes
; like metaclass=Meta, which are NOT base classes.
(class_definition
  name: (identifier) @inherit.child
  superclasses: (argument_list
    (expression) @inherit.parent))

; Decorator on a function: @decorator def func()
(decorated_definition
  (decorator (identifier) @decorates.target)
  definition: (function_definition
    name: (identifier) @decorates.source))

; Decorator on a class: @decorator class Foo
(decorated_definition
  (decorator (identifier) @decorates.target)
  definition: (class_definition
    name: (identifier) @decorates.source))

; FastAPI/Starlette middleware registration: app.add_middleware(Cls) or
; app.add_middleware(Cls, dispatch=fn). The dispatch kwarg names the actual
; middleware function; without it, the class itself is the target (its
; __call__/dispatch method isn't resolvable from the call site alone, but
; recording the class keeps the registration visible instead of silently
; dropped). AIF3X-331 #16: this is what makes add_middleware(...) show up
; via trace_callers on the registered symbol, instead of only unit-test
; callers.
(call
  function: (attribute
    attribute: (identifier) @_method)
  arguments: (argument_list
    (identifier) @middleware.target
    (keyword_argument
      name: (identifier) @_kw
      value: (identifier) @middleware.target)?)
  (#eq? @_method "add_middleware")) @middleware.site

; FastAPI Depends() wiring, both shapes:
;   def handler(x = Depends(fn)): ...                        (parameter default)
;   APIRouter(dependencies=[Depends(fn), ...])                (router/include_router)
; fn is captured as the dependency target; source is the enclosing function
; for the parameter-default form, or the file for the router-registration
; form (there is no enclosing function at module scope).
(default_parameter
  value: (call
    function: (identifier) @_fn
    arguments: (argument_list (identifier) @depends.target))
  (#eq? @_fn "Depends")) @depends.site

(keyword_argument
  name: (identifier) @_kw
  value: (list
    (call
      function: (identifier) @_fn
      arguments: (argument_list (identifier) @depends.target))+)
  (#eq? @_kw "dependencies")
  (#eq? @_fn "Depends")) @depends.site
