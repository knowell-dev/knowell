; The default export of a `pages/api/**` file handles the route.

((export_statement
  declaration: (function_declaration name: (identifier) @handler)) @export
  (#match? @export "^export\\s+default\\s"))

((export_statement) @export
  (#match? @export "^export\\s+default\\s"))
