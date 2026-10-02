; Go declarations.

(function_declaration name: (identifier) @name body: (block) @body) @definition.function
(function_declaration name: (identifier) @name) @definition.function

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [
        (type_identifier) @receiver
        (pointer_type (type_identifier) @receiver)
        (generic_type type: (type_identifier) @receiver)
        (pointer_type (generic_type type: (type_identifier) @receiver))
      ]))
  name: (field_identifier) @name
  body: (block) @body) @definition.method

(type_declaration
  (type_spec name: (type_identifier) @name type: (struct_type (field_declaration_list) @body)) @definition.struct)
(type_declaration
  (type_spec name: (type_identifier) @name type: (interface_type)) @definition.interface)
(type_declaration
  (type_spec name: (type_identifier) @name) @definition.type_alias)
(type_declaration
  (type_alias name: (type_identifier) @name) @definition.type_alias)

(source_file (const_declaration (const_spec name: (identifier) @name) @definition.constant))
(source_file (var_declaration (var_spec name: (identifier) @name) @definition.variable))

(field_declaration_list
  (field_declaration name: (field_identifier) @name) @definition.field)
(interface_type
  (method_elem name: (field_identifier) @name) @definition.method)
