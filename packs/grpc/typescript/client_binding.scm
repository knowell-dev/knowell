; const client = new RouteServiceClient(address, credentials)

(variable_declarator
  name: (identifier) @name
  value: (new_expression
    constructor: [(identifier) @ctor (member_expression property: (property_identifier) @ctor)])
  (#match? @ctor "Client$"))
