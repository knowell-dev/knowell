; consumer.subscribe(&["a", "b"]) / consumer.subscribe(TOPICS)

(call_expression
  function: (field_expression field: (field_identifier) @_s)
  arguments: (arguments . (_) @topic)
  (#eq? @_s "subscribe"))
