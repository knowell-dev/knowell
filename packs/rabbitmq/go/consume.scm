; ch.Consume(queue, consumer, autoAck, exclusive, noLocal, noWait, args)

(call_expression
  function: (selector_expression field: (field_identifier) @_consume)
  arguments: (argument_list . (_) @topic . (_) . (_) . (_) . (_) . (_) . (_) .)
  (#eq? @_consume "Consume"))
