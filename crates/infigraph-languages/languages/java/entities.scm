; Java entity extraction queries

; Class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

; Annotated class declarations (@RestController, @Controller, @Service, @Component)
(class_declaration
  (modifiers
    (marker_annotation
      name: (identifier) @class.decorator))
  name: (identifier) @class.name) @class.def

; Annotated class with parameterized annotation (@RequestMapping("/api"))
(class_declaration
  (modifiers
    (annotation
      name: (identifier) @class.decorator))
  name: (identifier) @class.name) @class.def

; Interface declarations
(interface_declaration
  name: (identifier) @class.name) @class.def

; Enum declarations
(enum_declaration
  name: (identifier) @class.name) @class.def

; Method declarations
(class_declaration
  body: (class_body
    (method_declaration
      name: (identifier) @method.name) @method.def))

; Annotated method declarations (Spring @GetMapping, @PostMapping, etc.)
(class_declaration
  body: (class_body
    (method_declaration
      (modifiers
        (marker_annotation
          name: (identifier) @method.decorator))
      name: (identifier) @method.name) @method.def))

; Annotated methods with arguments (@RequestMapping(value="/path"))
(class_declaration
  body: (class_body
    (method_declaration
      (modifiers
        (annotation
          name: (identifier) @method.decorator
          arguments: (annotation_argument_list) @method.docstring))
      name: (identifier) @method.name) @method.def))

; Constructor declarations
(class_declaration
  body: (class_body
    (constructor_declaration
      name: (identifier) @method.name) @method.def))

; Interface method declarations
(interface_declaration
  body: (interface_body
    (method_declaration
      name: (identifier) @method.name) @method.def))

; Field declarations
(class_declaration
  body: (class_body
    (field_declaration
      declarator: (variable_declarator
        name: (identifier) @var.name)) @var.def))

; Test methods (JUnit convention)
(method_declaration
  (modifiers
    (marker_annotation
      name: (identifier) @_ann
      (#eq? @_ann "Test")))
  name: (identifier) @test.name) @test.def
