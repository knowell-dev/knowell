; channel.consume("queue", onMessage)

(call_expression
  function: (member_expression property: (property_identifier) @_consume)
  arguments: (arguments . [(string) (identifier) (member_expression)] @topic . (_))
  (#eq? @_consume "consume"))
