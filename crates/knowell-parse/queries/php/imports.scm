; `use` specifiers come from the statement text (handles `use function`,
; groups and aliases).
(namespace_use_declaration) @import
(require_expression (string (string_content) @import.source)) @import
(require_once_expression (string (string_content) @import.source)) @import
(include_expression (string (string_content) @import.source)) @import
(include_once_expression (string (string_content) @import.source)) @import
