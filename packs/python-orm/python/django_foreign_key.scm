; owner = models.ForeignKey(...) / models.OneToOneField(...) -> column owner_id

(class_definition
  name: (identifier) @entity
  superclasses: (argument_list (attribute object: (identifier) @_models attribute: (identifier) @_model))
  body: (block
    (expression_statement
      (assignment
        left: (identifier) @property
        right: (call
          function: (attribute object: (identifier) @_m attribute: (identifier) @_field)))))
  (#eq? @_models "models")
  (#eq? @_model "Model")
  (#eq? @_m "models")
  (#any-of? @_field "ForeignKey" "OneToOneField"))
