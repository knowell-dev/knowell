; Kotlin declarations. `class_declaration` covers classes, interfaces and
; enum classes; the keyword / body type tells them apart.

(class_declaration "interface" name: (identifier) @name (class_body) @body) @definition.interface
(class_declaration "interface" name: (identifier) @name) @definition.interface
(class_declaration name: (identifier) @name (enum_class_body) @body) @definition.enum
(class_declaration name: (identifier) @name (class_body) @body) @definition.class
(class_declaration name: (identifier) @name) @definition.class
(object_declaration name: (identifier) @name (class_body) @body) @definition.class
(object_declaration name: (identifier) @name) @definition.class

(function_declaration name: (identifier) @name (function_body) @body) @definition.function
(function_declaration name: (identifier) @name) @definition.function
(secondary_constructor "constructor" @name) @definition.constructor
(type_alias type: (identifier) @name) @definition.type_alias

(source_file
  (property_declaration (variable_declaration (identifier) @name)) @definition.variable)
(class_body
  (property_declaration (variable_declaration (identifier) @name)) @definition.field)
