; process.env.NAME
(member_expression
  object: (member_expression
    object: (identifier) @_root
    property: (property_identifier) @_env)
  property: (property_identifier) @name
  (#eq? @_root "process")
  (#eq? @_env "env"))

; import.meta.env.NAME (Vite)
(member_expression
  object: (member_expression
    object: (meta_property)
    property: (property_identifier) @_env)
  property: (property_identifier) @name
  (#eq? @_env "env"))
