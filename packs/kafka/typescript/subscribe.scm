; consumer.subscribe({ topic: "x" }) / consumer.subscribe({ topics: ["a", "b"] })

(call_expression
  function: (member_expression property: (property_identifier) @_sub)
  arguments: (arguments . (object
    (pair key: (property_identifier) @_t value: (_) @topic)))
  (#eq? @_sub "subscribe")
  (#any-of? @_t "topic" "topics"))
