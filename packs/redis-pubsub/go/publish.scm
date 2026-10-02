; rdb.Publish(ctx, "channel", message)

(call_expression
  function: (selector_expression field: (field_identifier) @_publish)
  arguments: (argument_list . (_) . [(interpreted_string_literal) (raw_string_literal) (identifier) (selector_expression)] @topic . (_) .)
  (#eq? @_publish "Publish"))
