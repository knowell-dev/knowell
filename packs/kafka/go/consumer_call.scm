; Heuristic: newReader(brokers, "x"), NewConsumer(..., "x"), c.Subscribe("x", nil),
; c.SubscribeTopics([]string{"x"}, nil), consumer.ConsumePartition("x", 0, offset)

(call_expression
  function: [(identifier) @_fn (selector_expression field: (field_identifier) @_fn)]
  arguments: (argument_list [(interpreted_string_literal) (raw_string_literal)] @topic)
  (#match? @_fn "(?i)^(new[a-z0-9_]*(reader|consumer|subscriber)|subscribe|consumepartition)$"))

(call_expression
  function: (selector_expression field: (field_identifier) @_fn)
  arguments: (argument_list . (composite_literal) @topic)
  (#eq? @_fn "SubscribeTopics"))
