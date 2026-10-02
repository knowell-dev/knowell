; app.MapGet("/x", handler) and friends.

(invocation_expression
  function: (member_access_expression name: (identifier) @verb)
  arguments: (argument_list . (argument (string_literal) @path) . (argument))
  (#any-of? @verb "MapGet" "MapPost" "MapPut" "MapPatch" "MapDelete"))
