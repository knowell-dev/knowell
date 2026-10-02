; nc.Publish("subject", data) / js.Publish("subject", data, opts...)

(call_expression
  function: (selector_expression field: (field_identifier) @_publish)
  arguments: (argument_list . [(interpreted_string_literal) (raw_string_literal) (identifier) (selector_expression)] @topic . (_))
  (#any-of? @_publish "Publish" "PublishMsg" "PublishAsync"))
