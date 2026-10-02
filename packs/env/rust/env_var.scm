; std::env::var("NAME") / env::var_os("NAME") / env::var(name)
(call_expression
  function: (scoped_identifier
    path: [(identifier) @_env (scoped_identifier name: (identifier) @_env)]
    name: (identifier) @_var)
  arguments: (arguments . (_) @name)
  (#eq? @_env "env")
  (#any-of? @_var "var" "var_os"))

; env!("NAME") / option_env!("NAME")
(macro_invocation
  macro: (identifier) @_macro
  (token_tree . (string_literal) @name)
  (#any-of? @_macro "env" "option_env"))
