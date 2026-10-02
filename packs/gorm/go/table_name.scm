; func (Forklift) TableName() string { return "forklifts" }

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: [(type_identifier) @model (pointer_type (type_identifier) @model)]))
  name: (field_identifier) @_name
  body: (block
    (statement_list
      (return_statement
        (expression_list . (interpreted_string_literal) @table))))
  (#eq? @_name "TableName"))
