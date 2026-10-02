; process.env["NAME"] / process.env[name] (a variable index is unresolved)
(subscript_expression
  object: (member_expression
    object: (identifier) @_root
    property: (property_identifier) @_env)
  index: (_) @name
  (#eq? @_root "process")
  (#eq? @_env "env"))
