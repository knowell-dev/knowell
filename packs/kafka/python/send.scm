; producer.send("x", value) / await producer.send_and_wait(TOPIC, value) / producer.produce("x", ...)

(call
  function: (attribute object: (_) @_producer attribute: (identifier) @_send)
  arguments: (argument_list . [(string) (identifier) (attribute)] @topic)
  (#any-of? @_send "send" "send_and_wait" "produce")
  (#match? @_producer "(?i)producer"))
