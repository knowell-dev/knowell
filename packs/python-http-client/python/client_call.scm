; requests.get(url), httpx.post(...), client.get("/x"), session.delete(f"/x/{id}")
; The URL must be a string (or a concatenation / f-string); a call whose first
; argument is a plain name (SQLAlchemy `session.get(Model, id)`) is not HTTP.

((call
  function: (attribute object: (_) @_client attribute: (identifier) @verb)
  arguments: (argument_list . [(string) (binary_operator) (concatenated_string)] @url)) @call
  (#any-of? @verb "get" "post" "put" "patch" "delete" "head" "options")
  (#match? @_client "(^|\\.)_?(requests|httpx|client|session|http|api|[a-z_]+_client|[a-z_]+_session)$"))
