([(string_literal) (raw_string_literal)] @sql
  (#match? @sql "^r?#*\"\\s*(SELECT|INSERT|UPDATE|DELETE|WITH)\\s"))
