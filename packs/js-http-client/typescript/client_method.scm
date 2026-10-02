; axios.get("/x"), api.post(`/x/${id}`, body), ky.delete("https://...").

((call_expression
  function: (member_expression
    property: (property_identifier) @verb)
  arguments: (arguments . [(string) (template_string)] @url)) @call
  (#any-of? @verb "get" "post" "put" "patch" "delete" "head" "options")
  (#match? @url "^[\"'`](/|https?:|\\$\\{)"))
