; chi r.Get("/x", h), gin r.GET("/x", h), echo e.PUT("/x", h): a literal path
; followed by a handler (a one-argument `cache.Get("/x")` is not a route).

(call_expression
  function: (selector_expression
    operand: (_) @receiver
    field: (field_identifier) @verb)
  arguments: (argument_list . (interpreted_string_literal) @path . (_))
  (#any-of? @verb "Get" "Post" "Put" "Patch" "Delete" "Head" "Options" "GET" "POST" "PUT" "PATCH" "DELETE" "HEAD" "OPTIONS" "Any")
  (#match? @path "^\"/"))
