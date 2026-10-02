; Environment.GetEnvironmentVariable("NAME") / System.Environment.GetEnvironmentVariable("NAME")
(invocation_expression
  function: (member_access_expression
    name: (identifier) @_get)
  arguments: (argument_list . (argument (string_literal) @name))
  (#eq? @_get "GetEnvironmentVariable"))
