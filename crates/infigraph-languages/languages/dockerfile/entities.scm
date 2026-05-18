; Dockerfile entity extraction queries

; FROM instructions (base images)
(from_instruction
  (image_spec
    (image_name) @section.name)) @section.def

; LABEL key-value pairs
(label_instruction
  (label_pair
    key: (unquoted_string) @var.name)) @var.def

; ARG instructions
(arg_instruction
  (arg_pair
    name: (unquoted_string) @var.name)) @var.def

; ENV instructions
(env_instruction
  (env_pair
    name: (unquoted_string) @var.name)) @var.def
