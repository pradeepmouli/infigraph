; Ruby entity extraction queries

; Method definitions
(method
  name: (identifier) @method.name) @method.def

; Singleton method definitions (self.method)
(singleton_method
  name: (identifier) @method.name) @method.def

; Class definitions
(class
  name: (constant) @class.name) @class.def

; Module definitions
(module
  name: (constant) @class.name) @class.def

; Constant assignments
(assignment
  left: (constant) @var.name) @var.def

; === HTTP Route Patterns (Rails, Sinatra, Grape) ===

; get "/path" do ... end / post "/path" do ... end (Sinatra)
(call
  method: (identifier) @route.method
  arguments: (argument_list
    (string) @route.path)
  (#match? @route.method "^(get|post|put|patch|delete|options|head|match)$")) @route.def

; get "/path", to: "controller#action" (Rails)
(call
  method: (identifier) @route.method
  arguments: (argument_list
    (string) @route.path)
  (#match? @route.method "^(get|post|put|patch|delete|root|resources|resource|namespace|scope|mount|match)$")) @route.def
