; Dart declarations.

(class_declaration name: (identifier) @name body: (class_body) @body) @definition.class
(mixin_declaration name: (identifier) @name body: (class_body) @body) @definition.trait
(extension_declaration name: (identifier) @name body: (extension_body) @body) @definition.impl
(enum_declaration name: (identifier) @name body: (enum_body) @body) @definition.enum
(type_alias (type_identifier) @name) @definition.type_alias

(function_declaration
  signature: (function_signature name: (identifier) @name)
  body: (function_body) @body) @definition.function

(method_declaration
  signature: (method_signature [
    (function_signature name: (identifier) @name)
    (getter_signature name: (identifier) @name)
    (setter_signature name: (identifier) @name)
    (operator_signature) @name
  ])
  body: (function_body) @body) @definition.method
(method_declaration
  signature: (method_signature [
    (constructor_signature name: (identifier) @name)
    (factory_constructor_signature) @name
  ])
  body: (function_body) @body) @definition.constructor

(declaration [
  (constructor_signature name: (identifier) @name)
  (constant_constructor_signature) @name
  (factory_constructor_signature) @name
  (redirecting_factory_constructor_signature) @name
]) @definition.constructor
(declaration [
  (function_signature name: (identifier) @name)
  (getter_signature name: (identifier) @name)
  (setter_signature name: (identifier) @name)
]) @definition.method
(declaration
  (initialized_identifier_list
    (initialized_identifier name: (identifier) @name))) @definition.field
