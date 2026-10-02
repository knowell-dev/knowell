; Rust declarations. Patterns with a @body come before their body-less
; variants: when several patterns match one node, the first one wins.

(function_item name: (identifier) @name body: (block) @body) @definition.function
(function_signature_item name: (identifier) @name) @definition.function

(struct_item name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(struct_item name: (type_identifier) @name) @definition.struct
(union_item name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(enum_item name: (type_identifier) @name body: (enum_variant_list) @body) @definition.enum
(trait_item name: (type_identifier) @name body: (declaration_list) @body) @definition.trait
(impl_item type: (_) @name body: (declaration_list) @body) @definition.impl
(mod_item name: (identifier) @name body: (declaration_list) @body) @definition.module

(type_item name: (type_identifier) @name) @definition.type_alias
(associated_type name: (type_identifier) @name) @definition.type_alias
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(macro_definition name: (identifier) @name) @definition.macro

(field_declaration_list
  (field_declaration name: (field_identifier) @name) @definition.field)
