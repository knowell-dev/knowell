; Heuristic: getenv("NAME", fallback) / mustEnv("NAME") helpers wrapping os.Getenv.
(call_expression
  function: (identifier) @_fn
  arguments: (argument_list . (interpreted_string_literal) @name)
  (#match? @_fn "(?i)^(get_?env|must_?env|must_?get_?env|require_?env|env_?or|env_?(string|int|bool|duration)|lookup_?env_?or)$"))
