; Perl relationship extraction queries

; Function calls
(call_expression_with_bareword
  function_name: (identifier) @call.func) @call.site

; Use/no statements (module imports)
(use_no_statement
  package_name: (package_name) @import.module)

; Require statements
(require_statement
  package_name: (package_name) @import.module)
