; Heuristic: required("NAME") / optional("NAME", "default") / env_or("NAME", ..) helpers.
(call_expression
  function: (identifier) @_fn
  arguments: (arguments . (string_literal) @name)
  (#match? @_fn "(?i)^(required|optional|get_?env|must_?env|require_?env|env_?or|env_?var|read_?env)$"))
