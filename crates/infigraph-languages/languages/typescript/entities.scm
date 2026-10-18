; TypeScript entity extraction queries

; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def

; Arrow functions assigned to const/let
(lexical_declaration
  (variable_declarator
    name: (identifier) @func.name
    value: (arrow_function)) @func.def)

; Class declarations
(class_declaration
  name: (type_identifier) @class.name) @class.def

; Method definitions in classes
(method_definition
  name: (property_identifier) @method.name) @method.def

; Interface declarations
(interface_declaration
  name: (type_identifier) @class.name) @class.def

; Enum declarations
(enum_declaration
  name: (identifier) @class.name) @class.def

; Type alias declarations
(type_alias_declaration
  name: (type_identifier) @class.name) @class.def

; Variable/const declarations (non-arrow)
(variable_declarator
  name: (identifier) @var.name) @var.def

; === HTTP Route Patterns (Express, Koa, Hono, Fastify, NestJS) ===

; router.get("/path", handler) — with named handler
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (string) @route.path
      (identifier) @route.handler)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; router.get("/path", ...) — anonymous handler
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (string) @route.path)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; Template literal paths
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (template_string) @route.path)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

; === Next.js App Router ===
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
; is the TypeScript half of the parent-capture idiom (@method.parent) for
; fields and properties.

; const X = { ... } at module level (also `as` / `satisfies` wrappers, `var`,
; and `export`). A literal bound inside a function has no stable owner name --
; the same `const node = {..}` recurs in every function -- so it gets none.
(program
  [(lexical_declaration
     (variable_declarator
       name: (identifier) @member.root
       value: [(object) @member.scope
                  (as_expression . (object) @member.scope)
                  (satisfies_expression . (object) @member.scope)]))
   (variable_declaration
     (variable_declarator
       name: (identifier) @member.root
       value: [(object) @member.scope
                  (as_expression . (object) @member.scope)
                  (satisfies_expression . (object) @member.scope)]))
   (export_statement
     declaration: [(lexical_declaration
                     (variable_declarator
                       name: (identifier) @member.root
                       value: [(object) @member.scope
                                  (as_expression . (object) @member.scope)
                                  (satisfies_expression . (object) @member.scope)]))
                   (variable_declaration
                     (variable_declarator
                       name: (identifier) @member.root
                       value: [(object) @member.scope
                                  (as_expression . (object) @member.scope)
                                  (satisfies_expression . (object) @member.scope)]))])])

; a key whose value is itself a literal: { b: { ... } }
(object
  (pair
    key: (_) @member.owner
    value: [(object) @member.scope
            (as_expression . (object) @member.scope)
            (satisfies_expression . (object) @member.scope)])) @member.within

; every member of a literal
(object
  [(pair key: (_) @member.name)
   (shorthand_property_identifier) @member.name
   (method_definition name: (_) @member.name)]) @member.within
