; nc.publish("subject", data) / js.publish("subject", data)

(call_expression
  function: (member_expression property: (property_identifier) @_publish)
  arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic)
  (#eq? @_publish "publish"))
