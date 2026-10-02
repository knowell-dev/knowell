; const val NAME = "x" (top level or companion object)
(property_declaration
  (modifiers) @_mods
  (variable_declaration (identifier) @name)
  (string_literal) @value
  (#match? @_mods "const"))
