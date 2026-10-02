; nc.subscribe("subject", opts)

(call_expression
  function: (member_expression property: (property_identifier) @_subscribe)
  arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic)
  (#eq? @_subscribe "subscribe"))
