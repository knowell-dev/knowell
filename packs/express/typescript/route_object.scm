; fastify.route({ method: "PUT", url: "/x", handler }) in either key order.

(call_expression
  function: (member_expression property: (property_identifier) @_route)
  arguments: (arguments . (object
    (pair key: (property_identifier) @_m value: (string) @method)
    (pair key: (property_identifier) @_u value: (string) @url)))
  (#eq? @_route "route")
  (#eq? @_m "method")
  (#eq? @_u "url"))

(call_expression
  function: (member_expression property: (property_identifier) @_route)
  arguments: (arguments . (object
    (pair key: (property_identifier) @_u value: (string) @url)
    (pair key: (property_identifier) @_m value: (string) @method)))
  (#eq? @_route "route")
  (#eq? @_m "method")
  (#eq? @_u "url"))
