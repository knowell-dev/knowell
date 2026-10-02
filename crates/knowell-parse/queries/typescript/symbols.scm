; TypeScript / TSX declarations.

(function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(generator_function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(function_signature name: (identifier) @name) @definition.function

(class_declaration name: (type_identifier) @name body: (class_body) @body) @definition.class
(abstract_class_declaration name: (type_identifier) @name body: (class_body) @body) @definition.class
(interface_declaration name: (type_identifier) @name body: (interface_body) @body) @definition.interface
(enum_declaration name: (identifier) @name body: (enum_body) @body) @definition.enum
(type_alias_declaration name: (type_identifier) @name) @definition.type_alias
(internal_module name: (_) @name body: (statement_block) @body) @definition.module
(module name: (_) @name body: (statement_block) @body) @definition.module

(method_definition name: (_) @name body: (statement_block) @body) @definition.method
(interface_body (method_signature name: (_) @name) @definition.method)
(abstract_method_signature name: (_) @name) @definition.method
(public_field_definition name: (_) @name) @definition.field
(interface_body (property_signature name: (_) @name) @definition.field)

; Functions bound to module-level names.
(program
  (lexical_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function body: (_) @body) (function_expression body: (_) @body)]) @definition.function))
(program
  (export_statement
    (lexical_declaration
      (variable_declarator
        name: (identifier) @name
        value: [(arrow_function body: (_) @body) (function_expression body: (_) @body)]) @definition.function)))
(program
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [(arrow_function body: (_) @body) (function_expression body: (_) @body)]) @definition.function))

; Module-level constants (requires / dynamic imports are imports, not symbols).
((program
  (lexical_declaration
    kind: "const"
    (variable_declarator name: (identifier) @name value: (_) @_value) @definition.constant))
  (#not-match? @_value "^(require|import|await import)[ ]*[(]"))
((program
  (export_statement
    (lexical_declaration
      kind: "const"
      (variable_declarator name: (identifier) @name value: (_) @_value) @definition.constant)))
  (#not-match? @_value "^(require|import|await import)[ ]*[(]"))

; Test blocks: describe / it / test (+ .only / .skip).
((call_expression
  function: [(identifier) @_fn (member_expression object: (identifier) @_fn)]
  arguments: (arguments . [(string (string_fragment) @name) (template_string) @name])) @definition.test
  (#match? @_fn "^(describe|it|test|suite|context)$"))
