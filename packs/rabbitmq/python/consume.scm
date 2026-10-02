; channel.basic_consume(queue="x", on_message_callback=...)
(call
  function: (attribute attribute: (identifier) @_consume)
  arguments: (argument_list
    (keyword_argument name: (identifier) @_k value: (_) @topic))
  (#eq? @_consume "basic_consume")
  (#eq? @_k "queue"))

; channel.basic_consume("x", callback)
(call
  function: (attribute attribute: (identifier) @_consume)
  arguments: (argument_list . (string) @topic)
  (#eq? @_consume "basic_consume"))
