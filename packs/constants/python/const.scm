; NAME = "x" at module level, and class-level constants.
(module
  (expression_statement
    (assignment left: (identifier) @name right: [(string) (list) (tuple) (identifier)] @value)))

(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @name right: [(string) (list) (tuple)] @value))))
