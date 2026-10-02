; Swift declarations. `class_declaration` covers class, actor, struct, enum
; and extension; `declaration_kind` tells them apart.

(class_declaration declaration_kind: "class" name: (_) @name body: (_) @body) @definition.class
(class_declaration declaration_kind: "actor" name: (_) @name body: (_) @body) @definition.class
(class_declaration declaration_kind: "struct" name: (_) @name body: (_) @body) @definition.struct
(class_declaration declaration_kind: "enum" name: (_) @name body: (_) @body) @definition.enum
(class_declaration declaration_kind: "extension" name: (_) @name body: (_) @body) @definition.impl
(protocol_declaration name: (type_identifier) @name body: (protocol_body) @body) @definition.interface

(function_declaration name: (simple_identifier) @name body: (function_body) @body) @definition.function
(protocol_function_declaration name: (simple_identifier) @name) @definition.method
(init_declaration name: "init" @name body: (function_body) @body) @definition.constructor
(init_declaration name: "init" @name) @definition.constructor
(typealias_declaration name: (type_identifier) @name) @definition.type_alias

(class_body
  (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @definition.field)
(protocol_body
  (protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @definition.field)
(source_file
  (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @definition.variable)
