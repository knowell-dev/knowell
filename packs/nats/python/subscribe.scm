; await nc.subscribe("subject", cb=handler)

(call
  function: (attribute attribute: (identifier) @_subscribe)
  arguments: (argument_list . [(string) (identifier) (attribute)] @topic)
  (#eq? @_subscribe "subscribe"))
