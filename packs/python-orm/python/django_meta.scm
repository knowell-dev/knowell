; class Trailer(models.Model): class Meta: db_table = "trailers"

(class_definition
  name: (identifier) @model
  body: (block
    (class_definition
      name: (identifier) @_meta
      body: (block
        (expression_statement
          (assignment left: (identifier) @_k right: (string) @table)))))
  (#eq? @_meta "Meta")
  (#eq? @_k "db_table"))
