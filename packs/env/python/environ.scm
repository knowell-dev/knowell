; os.environ["NAME"]
(subscript
  value: (attribute object: (identifier) @_os attribute: (identifier) @_environ)
  subscript: (_) @name
  (#eq? @_os "os")
  (#eq? @_environ "environ"))

; os.environ.get("NAME") / os.getenv("NAME")
(call
  function: (attribute
    object: (attribute object: (identifier) @_os attribute: (identifier) @_environ)
    attribute: (identifier) @_get)
  arguments: (argument_list . (_) @name)
  (#eq? @_os "os")
  (#eq? @_environ "environ")
  (#any-of? @_get "get" "setdefault"))

(call
  function: (attribute object: (identifier) @_os attribute: (identifier) @_getenv)
  arguments: (argument_list . (_) @name)
  (#eq? @_os "os")
  (#eq? @_getenv "getenv"))
