; @RequestMapping("/v1/x") or @RequestMapping(value|path = "/v1/x") on a class.

(class_declaration
  (modifiers
    (annotation
      name: (identifier) @_rm
      arguments: (annotation_argument_list . (string_literal) @prefix)))
  name: (identifier) @class
  (#eq? @_rm "RequestMapping"))

(class_declaration
  (modifiers
    (annotation
      name: (identifier) @_rm
      arguments: (annotation_argument_list
        (element_value_pair key: (identifier) @_k value: (string_literal) @prefix))))
  name: (identifier) @class
  (#eq? @_rm "RequestMapping")
  (#any-of? @_k "value" "path"))
