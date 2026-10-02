; http.NewRequest("POST", url, body)
((call_expression
  function: (selector_expression operand: (identifier) @_pkg field: (field_identifier) @_f)
  arguments: (argument_list . (_) @verb . (_) @url)) @call
  (#eq? @_pkg "http")
  (#eq? @_f "NewRequest"))

; http.NewRequestWithContext(ctx, http.MethodPost, url, body)
((call_expression
  function: (selector_expression operand: (identifier) @_pkg field: (field_identifier) @_f)
  arguments: (argument_list . (_) . (_) @verb . (_) @url)) @call
  (#eq? @_pkg "http")
  (#eq? @_f "NewRequestWithContext"))

; http.Get(url), http.Post(url, contentType, body), http.Head(url)
((call_expression
  function: (selector_expression operand: (identifier) @_pkg field: (field_identifier) @verb)
  arguments: (argument_list . (_) @url)) @call
  (#eq? @_pkg "http")
  (#any-of? @verb "Get" "Post" "Head"))
