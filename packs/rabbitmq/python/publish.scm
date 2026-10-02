; channel.basic_publish(exchange="", routing_key="x", body=...)

(call
  function: (attribute attribute: (identifier) @_publish)
  arguments: (argument_list
    (keyword_argument name: (identifier) @_k value: (_) @topic))
  (#eq? @_publish "basic_publish")
  (#eq? @_k "routing_key"))
