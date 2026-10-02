; static final String NAME = "x";
(field_declaration
  (modifiers) @_mods
  declarator: (variable_declarator name: (identifier) @name value: [(string_literal) (array_initializer)] @value)
  (#match? @_mods "static")
  (#match? @_mods "final"))
