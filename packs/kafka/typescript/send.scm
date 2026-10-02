; producer.send({ topic: "x", messages }) / producer.send({ topic, messages })

(call_expression
  function: (member_expression property: (property_identifier) @_send)
  arguments: (arguments . (object
    (pair key: (property_identifier) @_t value: (_) @topic)))
  (#any-of? @_send "send" "sendBatch")
  (#eq? @_t "topic"))

(call_expression
  function: (member_expression property: (property_identifier) @_send)
  arguments: (arguments . (object (shorthand_property_identifier) @topic))
  (#any-of? @_send "send" "sendBatch")
  (#eq? @topic "topic"))
