; Flask @app.route("/x") and @bp.route("/x", methods=["GET", "PATCH"]).

(decorated_definition
  (decorator
    (call
      function: (attribute
        object: (identifier) @router
        attribute: (identifier) @_route)
      arguments: (argument_list . (string) @path))) @decorator
  definition: (function_definition name: (identifier) @handler)
  (#eq? @_route "route"))

(decorated_definition
  (decorator
    (call
      function: (attribute
        object: (identifier) @router
        attribute: (identifier) @_route)
      arguments: (argument_list . (string) @path
        (keyword_argument name: (identifier) @_k value: (list) @methods)))) @decorator
  definition: (function_definition name: (identifier) @handler)
  (#eq? @_route "route")
  (#eq? @_k "methods"))
