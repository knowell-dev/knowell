; Java declarations.

(class_declaration name: (identifier) @name body: (class_body) @body) @definition.class
(record_declaration name: (identifier) @name body: (class_body) @body) @definition.class
(interface_declaration name: (identifier) @name body: (interface_body) @body) @definition.interface
(annotation_type_declaration name: (identifier) @name body: (annotation_type_body) @body) @definition.interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @definition.enum

(method_declaration name: (identifier) @name body: (block) @body) @definition.method
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name body: (constructor_body) @body) @definition.constructor
(compact_constructor_declaration name: (identifier) @name body: (block) @body) @definition.constructor

(field_declaration declarator: (variable_declarator name: (identifier) @name)) @definition.field
(constant_declaration declarator: (variable_declarator name: (identifier) @name)) @definition.constant
