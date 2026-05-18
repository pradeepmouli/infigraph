; Protobuf (proto3) entity extraction queries

; Message definitions
(message
  (message_name) @class.name) @class.def

; Enum definitions
(enum
  (enum_name) @class.name) @class.def

; Service definitions
(service
  (service_name) @class.name) @class.def

; RPC method definitions
(rpc
  (rpc_name) @method.name) @method.def
