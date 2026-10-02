; Every named field of a struct; `requires_rule = "model"` keeps only the
; fields of structs that carry a gorm tag. The column comes from the tag.

(type_spec
  name: (type_identifier) @model
  type: (struct_type
    (field_declaration_list
      (field_declaration
        name: (field_identifier) @field
        tag: (raw_string_literal)? @tag))))
