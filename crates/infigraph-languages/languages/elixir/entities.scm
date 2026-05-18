; Elixir entity extraction queries

; Module definitions: defmodule
(call
  target: (identifier) @_keyword
  (arguments
    (alias) @class.name)
  (#any-of? @_keyword "defmodule" "defprotocol")) @class.def

; Function definitions: def/defp
(call
  target: (identifier) @_keyword
  (arguments
    (call
      target: (identifier) @func.name))
  (#any-of? @_keyword "def" "defp")) @func.def

; Zero-arity function definitions
(call
  target: (identifier) @_keyword
  (arguments
    (identifier) @func.name)
  (#any-of? @_keyword "def" "defp")) @func.def

; Macro definitions
(call
  target: (identifier) @_keyword
  (arguments
    (call
      target: (identifier) @func.name))
  (#any-of? @_keyword "defmacro" "defmacrop")) @func.def

; Test definitions
(call
  target: (identifier) @_keyword
  (arguments
    (string
      (quoted_content) @test.name))
  (#eq? @_keyword "test")) @test.def

; === HTTP Route Patterns (Phoenix) ===

; get "/path", Controller, :action
(call
  target: (identifier) @route.method
  (arguments
    (string
      (quoted_content) @route.path))
  (#any-of? @route.method "get" "post" "put" "patch" "delete" "options" "head" "resources" "scope" "pipe_through" "forward")) @route.def
