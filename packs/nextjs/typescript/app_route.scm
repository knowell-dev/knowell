; Exported HTTP-method handlers of an app-router `route.ts` file.

(export_statement
  declaration: (function_declaration name: (identifier) @verb)
  (#any-of? @verb "GET" "POST" "PUT" "PATCH" "DELETE" "HEAD" "OPTIONS"))

(export_statement
  declaration: (lexical_declaration
    (variable_declarator name: (identifier) @verb))
  (#any-of? @verb "GET" "POST" "PUT" "PATCH" "DELETE" "HEAD" "OPTIONS"))
