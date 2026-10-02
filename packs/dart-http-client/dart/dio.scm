; dio.get('/x'), _dio.post<Map<String, dynamic>>('/x/$id', data: ...)

(call_expression
  function: [
    (member_expression object: (_) @_client property: (identifier) @verb)
    (instantiation_expression
      function: (member_expression object: (_) @_client property: (identifier) @verb))
  ]
  arguments: (arguments . (string_literal) @url)
  (#any-of? @verb "get" "post" "put" "patch" "delete" "head")
  (#match? @url "^r?['\"](/|https?:|\\$)"))
