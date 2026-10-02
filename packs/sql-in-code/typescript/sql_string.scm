([(string) (template_string)] @sql
  (#match? @sql "^[\"'`]\\s*(SELECT|INSERT|UPDATE|DELETE|WITH)\\s"))
