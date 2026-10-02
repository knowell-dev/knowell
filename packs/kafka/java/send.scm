; kafkaTemplate.send("x", payload) / producer.send(new ProducerRecord<>("x", value))

(method_invocation
  object: (_) @_target
  name: (identifier) @_send
  arguments: (argument_list . [(string_literal) (identifier) (field_access)] @topic)
  (#eq? @_send "send")
  (#match? @_target "(?i)(kafka|template|producer)"))

(object_creation_expression
  type: [(type_identifier) @_record (generic_type (type_identifier) @_record)]
  arguments: (argument_list . [(string_literal) (identifier) (field_access)] @topic)
  (#eq? @_record "ProducerRecord"))
