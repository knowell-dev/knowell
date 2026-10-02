; URL(string: "https://api.example.com/v1/x/\(id)")

(call_expression
  (simple_identifier) @_url
  (call_suffix
    (value_arguments
      (value_argument
        name: (value_argument_label (simple_identifier) @_label)
        value: (line_string_literal) @url)))
  (#eq? @_url "URL")
  (#eq? @_label "string"))
