; C# entity extraction queries

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Interface declarations
(interface_declaration
  name: (identifier) @class.name) @class.def

; Struct declarations
(struct_declaration
  name: (identifier) @class.name) @class.def

; Enum declarations
(enum_declaration
  name: (identifier) @class.name) @class.def

; Method declarations
(method_declaration
  name: (identifier) @method.name) @method.def

; Namespace declarations
(namespace_declaration
  name: (identifier) @class.name) @class.def

; Property declarations
(property_declaration
  name: (identifier) @var.name) @var.def

; === HTTP Route Patterns (ASP.NET Minimal APIs) ===

; app.MapGet("/path", handler) / app.MapPost / app.MapPut / app.MapDelete
(expression_statement
  (invocation_expression
    function: (member_access_expression
      name: (identifier) @route.method)
    arguments: (argument_list
      (argument
        (string_literal) @route.path))) @route.def
  (#match? @route.method "^(MapGet|MapPost|MapPut|MapDelete|MapPatch|MapGroup|Map)$"))
