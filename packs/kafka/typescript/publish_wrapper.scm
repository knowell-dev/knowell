; Heuristic: this.events.publish(SUBSCRIPTION_CANCELLED, event)

(call_expression
  function: (member_expression property: (property_identifier) @_publish)
  arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic . (_))
  (#any-of? @_publish "publish" "publishEvent" "emitEvent"))
