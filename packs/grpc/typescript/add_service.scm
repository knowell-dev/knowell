; server.addService(RouteServiceService, { getRoute: handler, ... })

(call_expression
  function: (member_expression property: (property_identifier) @_add)
  arguments: (arguments
    .
    (_) @service
    .
    (object (pair key: (property_identifier) @method)))
  (#eq? @_add "addService"))
