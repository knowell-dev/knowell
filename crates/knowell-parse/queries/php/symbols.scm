; PHP declarations.

(namespace_definition name: (namespace_name) @name body: (compound_statement) @body) @definition.module
(namespace_definition name: (namespace_name) @name) @definition.module

(class_declaration name: (name) @name body: (declaration_list) @body) @definition.class
(interface_declaration name: (name) @name body: (declaration_list) @body) @definition.interface
(trait_declaration name: (name) @name body: (declaration_list) @body) @definition.trait
(enum_declaration name: (name) @name body: (enum_declaration_list) @body) @definition.enum

(function_definition name: (name) @name body: (compound_statement) @body) @definition.function
(method_declaration name: (name) @name body: (compound_statement) @body) @definition.method
(method_declaration name: (name) @name) @definition.method

(const_declaration (const_element (name) @name)) @definition.constant
(property_declaration (property_element name: (variable_name) @name)) @definition.field
