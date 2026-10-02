; r.publish("channel", message)

(call
  function: (attribute attribute: (identifier) @_publish)
  arguments: (argument_list . [(string) (identifier) (attribute)] @topic . (_) .)
  (#eq? @_publish "publish"))
