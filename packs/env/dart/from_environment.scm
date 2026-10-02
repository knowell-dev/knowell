; String.fromEnvironment('NAME') / bool.fromEnvironment('NAME') / int.fromEnvironment('NAME')
(call_expression
  function: (member_expression
    object: (_) @_type
    property: (identifier) @_from)
  arguments: (arguments . (string_literal) @name)
  (#any-of? @_type "String" "bool" "int")
  (#eq? @_from "fromEnvironment"))

; Platform.environment['NAME']
(index_expression
  object: (member_expression object: (identifier) @_platform property: (identifier) @_environment)
  index: (string_literal) @name
  (#eq? @_platform "Platform")
  (#eq? @_environment "environment"))
