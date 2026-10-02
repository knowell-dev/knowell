(import_statement source: (string (string_fragment) @import.source)) @import
(export_statement source: (string (string_fragment) @import.source)) @import
((call_expression
  function: (identifier) @_fn
  arguments: (arguments . (string (string_fragment) @import.source))) @import
  (#eq? @_fn "require"))
(call_expression
  function: (import)
  arguments: (arguments . (string (string_fragment) @import.source))) @import
