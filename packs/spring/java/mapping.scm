; @GetMapping("/x"), @PostMapping, @PutMapping(value = "/x") on a method of a
; @RestController / @Controller class.

(class_declaration
  (modifiers [(marker_annotation name: (identifier) @_ctl) (annotation name: (identifier) @_ctl)])
  name: (identifier) @class
  body: (class_body
    (method_declaration
      (modifiers
        (annotation
          name: (identifier) @verb
          arguments: (annotation_argument_list . (string_literal) @path)) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#any-of? @verb "GetMapping" "PostMapping" "PutMapping" "PatchMapping" "DeleteMapping"))

(class_declaration
  (modifiers [(marker_annotation name: (identifier) @_ctl) (annotation name: (identifier) @_ctl)])
  name: (identifier) @class
  body: (class_body
    (method_declaration
      (modifiers
        (annotation
          name: (identifier) @verb
          arguments: (annotation_argument_list
            (element_value_pair key: (identifier) @_k value: (string_literal) @path))) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#any-of? @verb "GetMapping" "PostMapping" "PutMapping" "PatchMapping" "DeleteMapping")
  (#any-of? @_k "value" "path"))

(class_declaration
  (modifiers [(marker_annotation name: (identifier) @_ctl) (annotation name: (identifier) @_ctl)])
  name: (identifier) @class
  body: (class_body
    (method_declaration
      (modifiers (marker_annotation name: (identifier) @verb) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#any-of? @verb "GetMapping" "PostMapping" "PutMapping" "PatchMapping" "DeleteMapping"))
