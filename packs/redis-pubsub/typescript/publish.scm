; redis.publish("channel", message)

(call_expression
  function: (member_expression property: (property_identifier) @_publish)
  arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic . (_) .)
  (#eq? @_publish "publish"))
