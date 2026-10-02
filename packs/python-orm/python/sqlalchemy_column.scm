; Columns of a class with __tablename__: `x: Mapped[T] = mapped_column(...)`,
; `x = Column(...)`; a leading string argument renames the column.

(class_definition
  name: (identifier) @entity
  body: (block
    (expression_statement
      (assignment left: (identifier) @_t right: (string) @table))
    (expression_statement
      (assignment
        left: (identifier) @property
        right: (call
          function: [(identifier) @_fn (attribute attribute: (identifier) @_fn)]
          arguments: (argument_list . (string) @column)))))
  (#eq? @_t "__tablename__")
  (#any-of? @_fn "mapped_column" "Column"))

(class_definition
  name: (identifier) @entity
  body: (block
    (expression_statement
      (assignment left: (identifier) @_t right: (string) @table))
    (expression_statement
      (assignment
        left: (identifier) @property
        right: (call
          function: [(identifier) @_fn (attribute attribute: (identifier) @_fn)]))))
  (#eq? @_t "__tablename__")
  (#any-of? @_fn "mapped_column" "Column"))
