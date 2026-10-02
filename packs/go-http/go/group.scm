; v2 := r.Group("/v2")  (gin, echo)

(short_var_declaration
  left: (expression_list . (identifier) @group)
  right: (expression_list . (call_expression
    function: (selector_expression field: (field_identifier) @_g)
    arguments: (argument_list . (interpreted_string_literal) @prefix)))
  (#eq? @_g "Group"))
