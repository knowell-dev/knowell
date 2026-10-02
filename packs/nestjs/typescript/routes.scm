; Route handlers of a @Controller class. Method decorators are siblings that
; precede the method in the class body, so the route decorator is followed by
; any number of other decorators (@HttpCode, @UseGuards) and then the method.

(export_statement
  decorator: (decorator
    (call_expression
      function: (identifier) @_controller
      arguments: (arguments . (string)? @prefix)))
  declaration: (class_declaration
    body: (class_body
      (decorator
        (call_expression
          function: (identifier) @verb
          arguments: (arguments . (string)? @path))) @route
      .
      (decorator)*
      .
      (method_definition name: (property_identifier) @handler)))
  (#eq? @_controller "Controller")
  (#any-of? @verb "Get" "Post" "Put" "Patch" "Delete" "Options" "Head" "All"))

(class_declaration
  decorator: (decorator
    (call_expression
      function: (identifier) @_controller
      arguments: (arguments . (string)? @prefix)))
  body: (class_body
    (decorator
      (call_expression
        function: (identifier) @verb
        arguments: (arguments . (string)? @path))) @route
    .
    (decorator)*
    .
    (method_definition name: (property_identifier) @handler))
  (#eq? @_controller "Controller")
  (#any-of? @verb "Get" "Post" "Put" "Patch" "Delete" "Options" "Head" "All"))
