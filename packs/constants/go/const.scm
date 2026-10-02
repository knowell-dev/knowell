; const Name = "x" / var names = []string{"a", "b"} (package level)
(source_file
  (const_declaration
    (const_spec name: (identifier) @name value: (expression_list . [(interpreted_string_literal) (raw_string_literal) (identifier)] @value))))

(source_file
  (var_declaration
    (var_spec name: (identifier) @name value: (expression_list . [(interpreted_string_literal) (raw_string_literal) (composite_literal)] @value))))
