; Python declarations. Functions nested in classes become methods.

(function_definition name: (identifier) @name body: (block) @body) @definition.function
(class_definition name: (identifier) @name body: (block) @body) @definition.class

; Module-level UPPER_CASE assignments.
((module
  (expression_statement
    (assignment left: (identifier) @name) @definition.constant))
  (#match? @name "^[A-Z_][A-Z0-9_]*$"))

; Class attributes.
(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @name) @definition.field)))
