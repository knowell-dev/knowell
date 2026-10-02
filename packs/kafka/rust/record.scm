; FutureRecord::to("x") / BaseRecord::to("x")

(call_expression
  function: (scoped_identifier
    path: (identifier) @_record
    name: (identifier) @_to)
  arguments: (arguments . (_) @topic)
  (#match? @_record "Record$")
  (#eq? @_to "to"))
