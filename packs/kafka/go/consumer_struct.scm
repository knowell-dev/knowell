; kafka.ReaderConfig{Topic: "x"} / kafka.ReaderConfig{GroupTopics: []string{"a", "b"}}

(composite_literal
  type: (qualified_type name: (type_identifier) @_type)
  body: (literal_value
    (keyed_element
      key: (literal_element (identifier) @_k)
      value: (literal_element (_) @topic)))
  (#eq? @_type "ReaderConfig")
  (#any-of? @_k "Topic" "GroupTopics"))
