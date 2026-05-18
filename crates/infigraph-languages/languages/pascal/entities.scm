; Pascal / Delphi entity extraction queries

; Unit / program / library declarations (module-level)
; moduleName is an unnamed child — capture via child pattern
(unit
  (moduleName
    (identifier) @module.name)) @module.def

(program
  (moduleName
    (identifier) @module.name)) @module.def

(library
  (moduleName
    (identifier) @module.name)) @module.def

; Type declarations: class, record, interface, object, enum, type alias
(declType
  name: (identifier) @class.name) @class.def

; Function / procedure / constructor / destructor / operator declarations
; defProc = full definition (header + body) — gives correct end_line
; declProc inside header gives the name
(defProc
  header: (declProc
    name: (identifier) @func.name)) @func.def

; Forward declarations (no body) — still capture with declProc for completeness
(declProc
  name: (identifier) @func.name) @func.def

; Variable declarations
(declVar
  name: (identifier) @var.name) @var.def

; Constant declarations
(declConst
  name: (identifier) @var.name) @var.def
