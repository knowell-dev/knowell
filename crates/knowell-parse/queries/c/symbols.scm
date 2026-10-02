; C declarations. Function names may sit under pointer declarators
; (char *name(void)).

(function_definition
  declarator: (function_declarator declarator: (identifier) @name)
  body: (compound_statement) @body) @definition.function
(function_definition
  declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
  body: (compound_statement) @body) @definition.function
(function_definition
  declarator: (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)))
  body: (compound_statement) @body) @definition.function

; Prototypes.
(declaration declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(declaration
  declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @definition.function

(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @definition.enum
(type_definition declarator: (type_identifier) @name) @definition.type_alias
(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro

(field_declaration_list
  (field_declaration declarator: (field_identifier) @name) @definition.field)
(field_declaration_list
  (field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @definition.field)
