([(string_literal) (verbatim_string_literal) (raw_string_literal)] @sql
  (#match? @sql "^@?\"+\\s*(SELECT|INSERT|UPDATE|DELETE|WITH)\\s"))
