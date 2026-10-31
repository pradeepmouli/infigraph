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

; === Object-literal member owners ===
; These emit NO symbol. extract_entities ignores @member.* captures; the SCIP
; import re-runs this query over a file (extract::literal_members) to learn who
; owns an object-literal member, which scip-typescript's monikers do not say
; (`ignorePatterns0:` has no owner, only a per-file counter). See
; crates/infigraph-core/src/extract/members.rs for the capture contract. This
; is the JavaScript half of the parent-capture idiom (@method.parent) for
; fields and properties.

; const X = { ... } at module level, with any `var` or `export`. The bound
; expression is captured whole and extract::members::literal_object finds the
; literal in it, through `as` / `satisfies` / parentheses. A literal bound
; inside a function has no stable owner name -- the same `const node = {..}`
; recurs in every function -- so it gets none.
(program
  [(lexical_declaration
     (variable_declarator
       name: (identifier) @member.root
       value: (_) @member.value))
   (variable_declaration
     (variable_declarator
       name: (identifier) @member.root
       value: (_) @member.value))
   (export_statement
     declaration: [(lexical_declaration
                     (variable_declarator
                       name: (identifier) @member.root
                       value: (_) @member.value))
                   (variable_declaration
                     (variable_declarator
                       name: (identifier) @member.root
                       value: (_) @member.value))])])

; a key whose value is itself a literal (or a literal under wrappers): { b: {..} }
(object
  (pair
    key: (_) @member.owner
    value: (_) @member.value)) @member.within

; every member of a literal
(object
  [(pair key: (_) @member.name)
   (shorthand_property_identifier) @member.name
   (method_definition name: (_) @member.name)]) @member.within
