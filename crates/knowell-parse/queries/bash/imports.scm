((command
  name: (command_name (word) @_command)
  argument: (_) @import.source) @import
  (#match? @_command "^(source|\\.)$"))
