; SQL schema objects. Statements themselves become blocks (in Rust code).

(create_table (object_reference name: (identifier) @name) (column_definitions) @body) @definition.table
(create_table (object_reference name: (identifier) @name)) @definition.table
(column_definitions (column_definition name: (identifier) @name) @definition.column)
(alter_table
  (object_reference name: (identifier) @receiver)
  (add_column (column_definition name: (identifier) @name) @definition.column))
(create_view (object_reference name: (identifier) @name)) @definition.view
(create_materialized_view (object_reference name: (identifier) @name)) @definition.view
(create_index column: (identifier) @name) @definition.index
(create_function (object_reference name: (identifier) @name)) @definition.function
(create_type (object_reference name: (identifier) @name)) @definition.type_alias
