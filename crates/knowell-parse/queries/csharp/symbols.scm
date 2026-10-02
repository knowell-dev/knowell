; C# declarations.

(namespace_declaration name: (_) @name body: (declaration_list) @body) @definition.module
(file_scoped_namespace_declaration name: (_) @name) @definition.module

(class_declaration name: (identifier) @name body: (declaration_list) @body) @definition.class
(record_declaration name: (identifier) @name body: (declaration_list) @body) @definition.class
(record_declaration name: (identifier) @name) @definition.class
(struct_declaration name: (identifier) @name body: (declaration_list) @body) @definition.struct
(interface_declaration name: (identifier) @name body: (declaration_list) @body) @definition.interface
(enum_declaration name: (identifier) @name body: (enum_member_declaration_list) @body) @definition.enum
(delegate_declaration name: (identifier) @name) @definition.type_alias

(method_declaration name: (identifier) @name body: (_) @body) @definition.method
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name body: (_) @body) @definition.constructor
(property_declaration name: (identifier) @name) @definition.field
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
