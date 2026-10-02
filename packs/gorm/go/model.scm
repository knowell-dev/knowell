; A struct with at least one `gorm:"..."` tag is a model.

(type_spec
  name: (type_identifier) @model
  type: (struct_type
    (field_declaration_list
      (field_declaration tag: (raw_string_literal) @_tag)))
  (#match? @_tag "gorm:\""))
