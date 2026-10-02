; os.Getenv("NAME") / os.LookupEnv("NAME") / os.Getenv(name)
(call_expression
  function: (selector_expression operand: (identifier) @_os field: (field_identifier) @_f)
  arguments: (argument_list . (_) @name)
  (#eq? @_os "os")
  (#any-of? @_f "Getenv" "LookupEnv"))
