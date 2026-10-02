; axios({ method: "delete", url: "/x" }) in either key order.

((call_expression
  function: (identifier) @_a
  arguments: (arguments . (object
    (pair key: (property_identifier) @_m value: (string) @verb)
    (pair key: (property_identifier) @_u value: [(string) (template_string)] @url)))) @call
  (#any-of? @_a "axios" "redaxios")
  (#eq? @_m "method")
  (#eq? @_u "url"))

((call_expression
  function: (identifier) @_a
  arguments: (arguments . (object
    (pair key: (property_identifier) @_u value: [(string) (template_string)] @url)
    (pair key: (property_identifier) @_m value: (string) @verb)))) @call
  (#any-of? @_a "axios" "redaxios")
  (#eq? @_m "method")
  (#eq? @_u "url"))
