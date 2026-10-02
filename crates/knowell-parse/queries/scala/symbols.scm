; Scala declarations.

(class_definition name: (identifier) @name body: (template_body) @body) @definition.class
(class_definition name: (identifier) @name) @definition.class
(object_definition name: (identifier) @name body: (template_body) @body) @definition.class
(object_definition name: (identifier) @name) @definition.class
(trait_definition name: (identifier) @name body: (template_body) @body) @definition.trait
(trait_definition name: (identifier) @name) @definition.trait
(enum_definition name: (identifier) @name body: (enum_body) @body) @definition.enum

(function_definition name: (identifier) @name body: (_) @body) @definition.function
(function_declaration name: (identifier) @name) @definition.function
(type_definition name: (type_identifier) @name) @definition.type_alias

(template_body (val_definition pattern: (identifier) @name) @definition.field)
(template_body (var_definition pattern: (identifier) @name) @definition.field)
(compilation_unit (val_definition pattern: (identifier) @name) @definition.constant)
(compilation_unit (var_definition pattern: (identifier) @name) @definition.variable)
