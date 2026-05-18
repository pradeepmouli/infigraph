; Clojure entity extraction queries

; Top-level forms (defn, def, defmacro, etc.)
(source
  (list_lit
    (sym_lit) @func.name)) @func.def

; === HTTP Route Patterns (Compojure, Ring) ===
(list_lit
  (sym_lit) @route.method
  (str_lit) @route.path
  (#match? @route.method "^(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS|ANY|context|defroutes)$")) @route.def
