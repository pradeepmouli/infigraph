; Haskell relationship extraction queries

; Function application (calls)
(apply
  (variable) @call.func) @call.site

; Import declarations
(import
  module: (module) @import.module)
