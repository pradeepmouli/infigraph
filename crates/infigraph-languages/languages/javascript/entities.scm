; JavaScript entity extraction queries

; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def

; Arrow functions assigned to variables
(lexical_declaration
  (variable_declarator
    name: (identifier) @func.name
    value: (arrow_function)) @func.def)

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Method definitions in classes
(class_declaration
  body: (class_body
    (method_definition
      name: (property_identifier) @method.name) @method.def))

; Variable/const declarations
(lexical_declaration
  (variable_declarator
    name: (identifier) @var.name)) @var.def

; Export default function
(export_statement
  (function_declaration
    name: (identifier) @func.name) @func.def)

; === HTTP Route Patterns (Express, Koa, Hono, Fastify) ===

; router.get("/path", handler) — with named handler
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (string) @route.path
      (identifier) @route.handler)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; router.get("/path", (req, res) => {}) — with anonymous handler
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (string) @route.path)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; router.get("/path", handler) — template literal path
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (template_string) @route.path)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; === Next.js App Router (export async function GET/POST/PUT/DELETE) ===
(export_statement
  (function_declaration
    name: (identifier) @route.method) @route.def
  (#match? @route.method "^(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)$"))
