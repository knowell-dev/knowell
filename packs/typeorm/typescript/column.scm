; Column-decorated properties of an @Entity class; the column name comes
; from `{ name: "..." }` when present (patterns with more captures win).

(export_statement
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_tn value: (string) @table))]?)))
  declaration: (class_declaration
    name: (type_identifier) @entity
    body: (class_body
      (public_field_definition
        decorator: (decorator (call_expression
          function: (identifier) @_column
          arguments: (arguments (object (pair key: (property_identifier) @_cn value: (string) @column)))))
        name: (property_identifier) @property)))
  (#eq? @_entity "Entity")
  (#match? @_column "^(Column|PrimaryColumn|PrimaryGeneratedColumn|CreateDateColumn|UpdateDateColumn|DeleteDateColumn|VersionColumn|ObjectIdColumn)$")
  (#eq? @_cn "name"))

(export_statement
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_tn value: (string) @table))]?)))
  declaration: (class_declaration
    name: (type_identifier) @entity
    body: (class_body
      (public_field_definition
        decorator: (decorator (call_expression function: (identifier) @_column))
        name: (property_identifier) @property)))
  (#eq? @_entity "Entity")
  (#match? @_column "^(Column|PrimaryColumn|PrimaryGeneratedColumn|CreateDateColumn|UpdateDateColumn|DeleteDateColumn|VersionColumn|ObjectIdColumn)$"))

(class_declaration
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_tn value: (string) @table))]?)))
  name: (type_identifier) @entity
  body: (class_body
    (public_field_definition
      decorator: (decorator (call_expression
        function: (identifier) @_column
        arguments: (arguments (object (pair key: (property_identifier) @_cn value: (string) @column)))))
      name: (property_identifier) @property))
  (#eq? @_entity "Entity")
  (#match? @_column "^(Column|PrimaryColumn|PrimaryGeneratedColumn|CreateDateColumn|UpdateDateColumn|DeleteDateColumn|VersionColumn|ObjectIdColumn)$")
  (#eq? @_cn "name"))

(class_declaration
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_tn value: (string) @table))]?)))
  name: (type_identifier) @entity
  body: (class_body
    (public_field_definition
      decorator: (decorator (call_expression function: (identifier) @_column))
      name: (property_identifier) @property))
  (#eq? @_entity "Entity")
  (#match? @_column "^(Column|PrimaryColumn|PrimaryGeneratedColumn|CreateDateColumn|UpdateDateColumn|DeleteDateColumn|VersionColumn|ObjectIdColumn)$"))
