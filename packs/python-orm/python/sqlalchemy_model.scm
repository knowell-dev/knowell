; class Gate(Base): __tablename__ = "gates"

(class_definition
  name: (identifier) @entity
  body: (block
    (expression_statement
      (assignment
        left: (identifier) @_t
        right: (string) @table)))
  (#eq? @_t "__tablename__"))
