; rabbitTemplate.convertAndSend("exchange", "routing.key", payload)
(method_invocation
  name: (identifier) @_send
  arguments: (argument_list . (_) @exchange . [(string_literal) (identifier) (field_access)] @topic . (_) .)
  (#eq? @_send "convertAndSend"))

; rabbitTemplate.convertAndSend("queue", payload)
(method_invocation
  name: (identifier) @_send
  arguments: (argument_list . [(string_literal) (identifier) (field_access)] @topic . (_) .)
  (#eq? @_send "convertAndSend"))
