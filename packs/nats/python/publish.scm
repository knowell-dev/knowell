; await nc.publish("subject", b"data")

(call
  function: (attribute attribute: (identifier) @_publish)
  arguments: (argument_list . [(string) (identifier) (attribute)] @topic)
  (#eq? @_publish "publish"))
