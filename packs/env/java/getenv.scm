; System.getenv("NAME")
(method_invocation
  object: (identifier) @_system
  name: (identifier) @_getenv
  arguments: (argument_list . (_) @name)
  (#eq? @_system "System")
  (#eq? @_getenv "getenv"))
