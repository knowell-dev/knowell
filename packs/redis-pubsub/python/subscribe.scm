; pubsub.subscribe("a", "b")

(call
  function: (attribute attribute: (identifier) @_subscribe)
  arguments: (argument_list (string) @topic)
  (#eq? @_subscribe "subscribe"))
