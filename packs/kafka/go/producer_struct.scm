; &kafka.Writer{Topic: "x"}, kafka.Message{Topic: "x"}, &sarama.ProducerMessage{Topic: "x"}

(composite_literal
  type: (qualified_type name: (type_identifier) @_type)
  body: (literal_value
    (keyed_element
      key: (literal_element (identifier) @_k)
      value: (literal_element (_) @topic)))
  (#any-of? @_type "Writer" "Message" "ProducerMessage")
  (#eq? @_k "Topic"))
