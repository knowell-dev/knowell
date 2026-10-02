; request.httpMethod = "POST"

(assignment
  target: (directly_assignable_expression
    (navigation_expression
      suffix: (navigation_suffix suffix: (simple_identifier) @property)))
  result: (line_string_literal) @method
  (#eq? @property "httpMethod"))
