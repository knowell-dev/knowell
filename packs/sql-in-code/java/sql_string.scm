((string_literal) @sql
  (#match? @sql "^(\"\"\"|\")\\s*(SELECT|INSERT|UPDATE|DELETE|WITH)\\s"))
