; @KafkaListener(topics = "x") / @KafkaListener(topics = {"a", "b"})

(method_declaration
  (modifiers
    (annotation
      name: (identifier) @_listener
      arguments: (annotation_argument_list
        (element_value_pair key: (identifier) @_k value: (_) @topic))))
  name: (identifier) @handler
  (#eq? @_listener "KafkaListener")
  (#eq? @_k "topics"))
