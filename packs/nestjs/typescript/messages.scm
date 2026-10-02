; Microservice handlers (@EventPattern / @MessagePattern) in a @Controller.

(export_statement
  decorator: (decorator
    (call_expression function: (identifier) @_controller))
  declaration: (class_declaration
    body: (class_body
      (decorator
        (call_expression
          function: (identifier) @_pattern
          arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic))) @route
      .
      (decorator)*
      .
      (method_definition name: (property_identifier) @handler)))
  (#eq? @_controller "Controller")
  (#any-of? @_pattern "EventPattern" "MessagePattern"))

(class_declaration
  decorator: (decorator
    (call_expression function: (identifier) @_controller))
  body: (class_body
    (decorator
      (call_expression
        function: (identifier) @_pattern
        arguments: (arguments . [(string) (template_string) (identifier) (member_expression)] @topic))) @route
    .
    (decorator)*
    .
    (method_definition name: (property_identifier) @handler))
  (#eq? @_controller "Controller")
  (#any-of? @_pattern "EventPattern" "MessagePattern"))
