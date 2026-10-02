; match message.topic() { "x" => ..., }

(match_expression
  value: (call_expression
    function: (field_expression field: (field_identifier) @_topic))
  body: (match_block
    (match_arm
      pattern: (match_pattern (string_literal) @topic)))
  (#eq? @_topic "topic"))
