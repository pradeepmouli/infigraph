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

; Method declarations, scoped to their enclosing type via @method.parent
; (issue #127's C# line item) so two same-named methods on different types in
; one file get distinct ids instead of colliding -- class_declaration/
; struct_declaration/interface_declaration all wrap their members in a
; declaration_list (verified against tree-sitter-c-sharp 0.23.5's real
; node-types.json), so this mirrors the same shape 3 times rather than
; relying on a single generic parent-kind wildcard.
(class_declaration
  name: (identifier) @method.parent
  body: (declaration_list
    (method_declaration name: (identifier) @method.name) @method.def))

(struct_declaration
  name: (identifier) @method.parent
  body: (declaration_list
    (method_declaration name: (identifier) @method.name) @method.def))

(interface_declaration
  name: (identifier) @method.parent
  body: (declaration_list
    (method_declaration name: (identifier) @method.name) @method.def))

; Constructor declarations — previously uncaptured, so every call made
; inside a constructor body (DI/init wiring, e.g. `myMainViewModel =
; new MainViewModel(); myMainViewModel.Initialize(...);`) had no enclosing
; symbol to attach the CALLS relation to. Named after their class in C#, so
; @method.name captures the class name itself, which is also correct for
; call-site attribution — a call inside `MainWin() { ... }` should be
; attributed to `MainWin::MainWin`, matching this file's class-scoped id
; convention. Scoped to @method.parent for the same reason as method
; declarations above; interfaces can't have constructors, so only class/struct.
(class_declaration
  name: (identifier) @method.parent
  body: (declaration_list
    (constructor_declaration name: (identifier) @method.name) @method.def))

(struct_declaration
  name: (identifier) @method.parent
  body: (declaration_list
    (constructor_declaration name: (identifier) @method.name) @method.def))

; Namespace declarations
(namespace_declaration
  name: (identifier) @class.name) @class.def

; Property declarations
(property_declaration
  name: (identifier) @var.name) @var.def

; Field declarations (`private Bar myBar;`) — previously uncaptured entirely,
; so calls made through a plain field (as opposed to an auto-property) never
; had a symbol to resolve the receiver's declared type against, e.g.
; `myBar.Initialize()` left receiver as the raw field name "myBar" with
; nothing in class_method_map to match it to Bar::Initialize.
(field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @var.name))) @var.def

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
