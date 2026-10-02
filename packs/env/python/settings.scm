; class Settings(BaseSettings): database_url: str  -> DATABASE_URL
(class_definition
  superclasses: (argument_list [(identifier) @_base (attribute attribute: (identifier) @_base)])
  body: (block
    (expression_statement
      (assignment left: (identifier) @field type: (type))))
  (#eq? @_base "BaseSettings")
  (#not-match? @field "^(model_config|Config)$"))
