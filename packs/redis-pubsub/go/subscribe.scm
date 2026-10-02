; rdb.Subscribe(ctx, "a", "b")

(call_expression
  function: (selector_expression field: (field_identifier) @_subscribe)
  arguments: (argument_list . (_) [(interpreted_string_literal) (raw_string_literal)] @topic)
  (#eq? @_subscribe "Subscribe"))
