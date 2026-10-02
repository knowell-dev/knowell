; Protocol Buffers declarations.

(service (service_name (identifier) @name)) @definition.service
(rpc (rpc_name (identifier) @name)) @definition.rpc
(message (message_name (identifier) @name) (message_body) @body) @definition.message
(enum (enum_name (identifier) @name) (enum_body) @body) @definition.enum

(message_body (field (identifier) @name) @definition.field)
(message_body (map_field (identifier) @name) @definition.field)
(oneof (oneof_field (identifier) @name) @definition.field)
(enum_body (enum_field (identifier) @name) @definition.field)
