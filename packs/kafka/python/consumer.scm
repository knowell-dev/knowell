; AIOKafkaConsumer("a", "b", group_id=...) / KafkaConsumer("a")
(call
  function: (identifier) @_c
  arguments: (argument_list (string) @topic)
  (#any-of? @_c "AIOKafkaConsumer" "KafkaConsumer"))

; consumer.subscribe(["a", "b"]) / consumer.subscribe(topics=["a"])
(call
  function: (attribute object: (_) @_consumer attribute: (identifier) @_s)
  arguments: (argument_list . [(list) (tuple) (string) (identifier)] @topic)
  (#eq? @_s "subscribe")
  (#match? @_consumer "(?i)consumer"))
