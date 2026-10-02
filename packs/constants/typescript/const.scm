; const NAME = "x";  export const NAME = ["a", "b"];  (module level only)
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name value: [(string) (template_string) (array) (identifier)] @value)))

(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name value: [(string) (template_string) (array) (identifier)] @value))))
