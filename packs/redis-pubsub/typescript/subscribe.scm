; subscriber.subscribe("a", "b") / subscriber.subscribe("a", listener)

(call_expression
  function: (member_expression property: (property_identifier) @_subscribe)
  arguments: (arguments [(string) (template_string)] @topic)
  (#eq? @_subscribe "subscribe"))
