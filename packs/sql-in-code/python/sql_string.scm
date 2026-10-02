((string) @sql
  (#match? @sql "^[a-zA-Z]*(\"\"\"|'''|\"|')\\s*(SELECT|INSERT|UPDATE|DELETE|WITH)\\s"))
