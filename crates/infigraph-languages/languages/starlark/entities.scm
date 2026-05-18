; Starlark/Bazel entity extraction queries

; Function definitions (def foo():)
(function_definition
  name: (identifier) @func.name) @func.def

; Rule calls at top level (cc_library, py_binary, etc.)
(call
  function: (identifier) @section.name) @section.def
