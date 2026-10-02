; mux.HandleFunc("GET /v1/x/{id}", h) / http.Handle("/x", h): the pattern may
; start with a method (Go 1.22); without one the route serves every method.

(call_expression
  function: (selector_expression field: (field_identifier) @_f)
  arguments: (argument_list . (interpreted_string_literal) @pattern . (_))
  (#any-of? @_f "HandleFunc" "Handle"))
