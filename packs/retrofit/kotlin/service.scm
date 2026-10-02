; @GET("v1/x/{id}") suspend fun get(...)

(function_declaration
  (modifiers
    (annotation
      (constructor_invocation
        (user_type (identifier) @verb)
        (value_arguments . (value_argument (string_literal) @path)))) @annotation)
  name: (identifier) @handler
  (#any-of? @verb "GET" "POST" "PUT" "PATCH" "DELETE" "HEAD" "OPTIONS"))
