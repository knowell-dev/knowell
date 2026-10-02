; @app.get("/x") / @router.post("") / @bp.delete("/x")

(decorated_definition
  (decorator
    (call
      function: (attribute
        object: (identifier) @router
        attribute: (identifier) @verb)
      arguments: (argument_list . (string) @path))) @decorator
  definition: [
    (function_definition name: (identifier) @handler)
  ]
  (#any-of? @verb "get" "post" "put" "patch" "delete" "head" "options"))
