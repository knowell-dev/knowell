; @Entity(), @Entity("name"), @Entity({ name: "name" }) on an (exported) class.

(export_statement
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_n value: (string) @table))]?)))
  declaration: (class_declaration name: (type_identifier) @entity)
  (#eq? @_entity "Entity"))

(class_declaration
  decorator: (decorator (call_expression
    function: (identifier) @_entity
    arguments: (arguments . [(string) @table (object (pair key: (property_identifier) @_n value: (string) @table))]?)))
  name: (type_identifier) @entity
  (#eq? @_entity "Entity"))
