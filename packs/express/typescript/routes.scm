; router.get("/path", handler...): a literal path followed by at least one
; more argument (a lookup such as `map.get("/x")` has only one).

(call_expression
  function: (member_expression
    property: (property_identifier) @verb)
  arguments: (arguments . (string) @path . (_))
  (#any-of? @verb "get" "post" "put" "patch" "delete" "options" "head" "all")
  (#match? @path "^[\"'`]/"))
