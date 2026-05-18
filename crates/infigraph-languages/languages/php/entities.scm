; PHP entity extraction queries

; Function definitions
(function_definition
  name: (name) @func.name) @func.def

; Class declarations
(class_declaration
  name: (name) @class.name) @class.def

; Interface declarations
(interface_declaration
  name: (name) @class.name) @class.def

; Trait declarations
(trait_declaration
  name: (name) @class.name) @class.def

; Method declarations
(method_declaration
  name: (name) @method.name) @method.def

; Namespace definitions
(namespace_definition
  name: (namespace_name) @class.name) @class.def

; Property declarations
(property_declaration
  (property_element
    (variable_name
      (name) @var.name))) @var.def

; === HTTP Route Patterns (Laravel, Slim, Symfony) ===

; Route::get("/path", ...) — scoped call expression
(scoped_call_expression
  name: (name) @route.method
  arguments: (arguments
    (argument
      (string
        (string_content) @route.path)))) @route.def

; $app->get("/path", handler) — member call expression (Slim)
(member_call_expression
  name: (name) @route.method
  arguments: (arguments
    (argument
      (string
        (string_content) @route.path)))) @route.def
