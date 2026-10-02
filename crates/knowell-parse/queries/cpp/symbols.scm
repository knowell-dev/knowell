; C++ declarations. Out-of-line member definitions (bool Service::cancel(...))
; are methods; the qualifier becomes the receiver in the qualified name.

(namespace_definition name: (_) @name body: (declaration_list) @body) @definition.module
(class_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.class
(struct_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list) @body) @definition.struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list) @body) @definition.enum

(function_definition
  declarator: (function_declarator declarator: (qualified_identifier) @name)
  body: (_) @body) @definition.method
(function_definition
  declarator: (pointer_declarator declarator: (function_declarator declarator: (qualified_identifier) @name))
  body: (_) @body) @definition.method
(function_definition
  declarator: (reference_declarator (function_declarator declarator: (qualified_identifier) @name))
  body: (_) @body) @definition.method
(function_definition
  declarator: (function_declarator
    declarator: [(identifier) (field_identifier) (operator_name) (destructor_name)] @name)
  body: (_) @body) @definition.function
(function_definition
  declarator: (pointer_declarator declarator: (function_declarator
    declarator: [(identifier) (field_identifier) (operator_name)] @name))
  body: (_) @body) @definition.function
(function_definition
  declarator: (reference_declarator (function_declarator
    declarator: [(identifier) (field_identifier) (operator_name)] @name))
  body: (_) @body) @definition.function

; Declarations without bodies (member declarations, prototypes).
(field_declaration
  declarator: (function_declarator
    declarator: [(field_identifier) (identifier) (operator_name) (destructor_name)] @name)) @definition.method
(field_declaration
  declarator: (pointer_declarator declarator: (function_declarator
    declarator: [(field_identifier) (identifier)] @name))) @definition.method
(field_declaration
  declarator: (reference_declarator (function_declarator
    declarator: [(field_identifier) (identifier)] @name))) @definition.method
(declaration
  declarator: (function_declarator
    declarator: [(identifier) (field_identifier) (destructor_name) (qualified_identifier)] @name)) @definition.function

(alias_declaration name: (type_identifier) @name) @definition.type_alias
(type_definition declarator: (type_identifier) @name) @definition.type_alias
(preproc_def name: (identifier) @name) @definition.macro
(preproc_function_def name: (identifier) @name) @definition.macro

(field_declaration_list
  (field_declaration declarator: (field_identifier) @name) @definition.field)
(field_declaration_list
  (field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @definition.field)
(field_declaration_list
  (field_declaration declarator: (reference_declarator (field_identifier) @name)) @definition.field)
