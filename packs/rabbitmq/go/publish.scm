; ch.Publish(exchange, key, mandatory, immediate, msg)
; ch.PublishWithContext(ctx, exchange, key, mandatory, immediate, msg)

(call_expression
  function: (selector_expression field: (field_identifier) @_publish)
  arguments: (argument_list . (_) @exchange . (_) @topic . (_) . (_) . (_) .)
  (#eq? @_publish "Publish"))

(call_expression
  function: (selector_expression field: (field_identifier) @_publish)
  arguments: (argument_list . (_) . (_) @exchange . (_) @topic . (_) . (_) . (_) .)
  (#eq? @_publish "PublishWithContext"))
