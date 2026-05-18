; Verilog/SystemVerilog relationship extraction queries

; Module instantiations
(module_instantiation
  (simple_identifier) @call.func) @call.site

; System task/function calls
(system_tf_call
  (system_tf_identifier) @call.func) @call.site
