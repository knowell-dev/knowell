; nc.Subscribe("subject", handler) / nc.QueueSubscribe("subject", "queue", handler)
; nc.ChanSubscribe("subject", ch)

(call_expression
  function: (selector_expression field: (field_identifier) @_subscribe)
  arguments: (argument_list . [(interpreted_string_literal) (raw_string_literal) (identifier) (selector_expression)] @topic . (_))
  (#any-of? @_subscribe "Subscribe" "QueueSubscribe" "ChanSubscribe" "SubscribeSync" "QueueSubscribeSync"))
