; @GetMapping("/x") / @PostMapping on a function of a @RestController class.

(class_declaration
  (modifiers (annotation [(user_type (identifier) @_ctl) (constructor_invocation (user_type (identifier) @_ctl))]))
  name: (identifier) @class
  (class_body
    (function_declaration
      (modifiers
        (annotation
          (constructor_invocation
            (user_type (identifier) @verb)
            (value_arguments . (value_argument (string_literal) @path)))) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#any-of? @verb "GetMapping" "PostMapping" "PutMapping" "PatchMapping" "DeleteMapping"))

(class_declaration
  (modifiers (annotation [(user_type (identifier) @_ctl) (constructor_invocation (user_type (identifier) @_ctl))]))
  name: (identifier) @class
  (class_body
    (function_declaration
      (modifiers
        (annotation (user_type (identifier) @verb)) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#any-of? @verb "GetMapping" "PostMapping" "PutMapping" "PatchMapping" "DeleteMapping"))
