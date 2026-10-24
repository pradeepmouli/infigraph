; TSX entity extraction queries (reuses TypeScript patterns)

; Function declarations
(function_declaration
  name: (identifier) @func.name) @func.def

; Class declarations
(class_declaration
  name: (type_identifier) @class.name) @class.def

; Abstract class declarations (tree-sitter-typescript's own node kind)
(abstract_class_declaration
  name: (type_identifier) @class.name) @class.def

; Properties of an interface or a type literal, and class fields. The owner
; (and whether there is one) comes from extract::ts_member_owner; a property
; with none -- a parameter's inline type -- is no symbol.
(property_signature
  name: (property_identifier) @field.name) @field.def

(public_field_definition
  name: (property_identifier) @field.name) @field.def

; Interface declarations
(interface_declaration
  name: (type_identifier) @class.name) @class.def

; Method definitions
(method_definition
  name: (property_identifier) @method.name) @method.def

; Variable declarations (const/let/var with arrow functions or values)
(lexical_declaration
  (variable_declarator
    name: (identifier) @var.name)) @var.def

; Type alias declarations
(type_alias_declaration
  name: (type_identifier) @class.name) @class.def

; Enum declarations
(enum_declaration
  name: (identifier) @class.name) @class.def

; === HTTP Route Patterns ===
(expression_statement
  (call_expression
    function: (member_expression
      property: (property_identifier) @route.method)
    arguments: (arguments
      (string) @route.path)) @route.def
  (#match? @route.method "^(get|post|put|delete|patch|options|head|all|use)$"))

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
