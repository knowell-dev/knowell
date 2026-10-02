; @GET("/v1/x") Call<List<X>> list();  (JAX-RS marker @GET has no path argument)

(method_declaration
  (modifiers
    (annotation
      name: (identifier) @verb
      arguments: (annotation_argument_list . (string_literal) @path)) @annotation)
  name: (identifier) @handler
  (#any-of? @verb "GET" "POST" "PUT" "PATCH" "DELETE" "HEAD" "OPTIONS"))
