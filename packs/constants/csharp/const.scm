; const string Name = "x";
(field_declaration
  (modifier) @_const
  (variable_declaration
    (variable_declarator name: (identifier) @name (string_literal) @value))
  (#eq? @_const "const"))
