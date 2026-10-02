((call
  method: (identifier) @_method
  arguments: (argument_list . (string (string_content) @import.source))) @import
  (#match? @_method "^(require|require_relative|load)$"))
