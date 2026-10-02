; fetch(url) and fetch(url, { method: "POST" }).

((call_expression
  function: (identifier) @_f
  arguments: (arguments . (_) @url)) @call
  (#eq? @_f "fetch"))

((call_expression
  function: (identifier) @_f
  arguments: (arguments . (_) @url . (object
    (pair key: (property_identifier) @_m value: (string) @verb)))) @call
  (#eq? @_f "fetch")
  (#eq? @_m "method"))
