; channel.publish("exchange", "routing.key", content)
(call_expression
  function: (member_expression property: (property_identifier) @_publish)
  arguments: (arguments . [(string) (identifier)] @exchange . [(string) (identifier) (member_expression)] @topic . (_))
  (#eq? @_publish "publish"))

; channel.sendToQueue("queue", content)
(call_expression
  function: (member_expression property: (property_identifier) @_send)
  arguments: (arguments . [(string) (identifier) (member_expression)] @topic . (_))
  (#eq? @_send "sendToQueue"))
