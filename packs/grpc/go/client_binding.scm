; client := pb.NewRouteServiceClient(conn)
(short_var_declaration
  left: (expression_list . (identifier) @name)
  right: (expression_list . (call_expression
    function: (selector_expression field: (field_identifier) @ctor)))
  (#match? @ctor "^New.+Client$"))

; &Client{stub: pb.NewRouteServiceClient(conn)}
(keyed_element
  key: (literal_element (identifier) @name)
  value: (literal_element (call_expression
    function: (selector_expression field: (field_identifier) @ctor)))
  (#match? @ctor "^New.+Client$"))
