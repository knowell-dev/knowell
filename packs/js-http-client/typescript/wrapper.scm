; Heuristic: API wrappers called with a literal path argument.
; request(BASE, "/v1/x"), request<T>(BASE, `/v1/x/${id}`, { method: "POST" })

((call_expression
  function: (identifier) @_fn
  arguments: (arguments [(string) (template_string)] @url)) @call
  (#match? @_fn "^(request|apiRequest|apiFetch|fetchJson|fetchApi|httpRequest|callApi|api)$")
  (#match? @url "^[\"'`]/"))

((call_expression
  function: (identifier) @_fn
  arguments: (arguments
    [(string) (template_string)] @url
    .
    (object (pair key: (property_identifier) @_m value: (string) @verb)))) @call
  (#match? @_fn "^(request|apiRequest|apiFetch|fetchJson|fetchApi|httpRequest|callApi|api)$")
  (#eq? @_m "method")
  (#match? @url "^[\"'`]/"))

; this.get("/x") / this.post(`/x/${id}`, body) inside an API client class.
((call_expression
  function: (member_expression
    object: (this)
    property: (property_identifier) @verb)
  arguments: (arguments . [(string) (template_string)] @url)) @call
  (#any-of? @verb "get" "post" "put" "patch" "delete")
  (#match? @url "^[\"'`]/"))
