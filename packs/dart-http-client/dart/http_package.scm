; http.get(Uri.parse('https://.../x')), client.post(Uri.parse('$base/x'), body: ...)

(call_expression
  function: (member_expression object: (_) @_client property: (identifier) @verb)
  arguments: (arguments
    .
    (call_expression
      function: (member_expression object: (identifier) @_uri property: (identifier) @_parse)
      arguments: (arguments . (string_literal) @url)))
  (#any-of? @verb "get" "post" "put" "patch" "delete" "head")
  (#eq? @_uri "Uri")
  (#any-of? @_parse "parse" "tryParse"))
