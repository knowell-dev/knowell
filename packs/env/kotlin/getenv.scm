; System.getenv("NAME")
(call_expression
  (navigation_expression
    (identifier) @_system
    (identifier) @_getenv)
  (value_arguments . (value_argument (string_literal) @name))
  (#eq? @_system "System")
  (#eq? @_getenv "getenv"))
