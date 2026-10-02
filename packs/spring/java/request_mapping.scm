; @RequestMapping(value = "/x", method = RequestMethod.PUT) on a controller method.

(class_declaration
  (modifiers [(marker_annotation name: (identifier) @_ctl) (annotation name: (identifier) @_ctl)])
  name: (identifier) @class
  body: (class_body
    (method_declaration
      (modifiers
        (annotation
          name: (identifier) @_rm
          arguments: (annotation_argument_list
            (element_value_pair key: (identifier) @_k value: (string_literal) @path))) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#eq? @_rm "RequestMapping")
  (#any-of? @_k "value" "path"))

(class_declaration
  (modifiers [(marker_annotation name: (identifier) @_ctl) (annotation name: (identifier) @_ctl)])
  name: (identifier) @class
  body: (class_body
    (method_declaration
      (modifiers
        (annotation
          name: (identifier) @_rm
          arguments: (annotation_argument_list
            (element_value_pair key: (identifier) @_k value: (string_literal) @path)
            (element_value_pair key: (identifier) @_mk value: (_) @method))) @mapping)
      name: (identifier) @handler))
  (#any-of? @_ctl "RestController" "Controller")
  (#eq? @_rm "RequestMapping")
  (#any-of? @_k "value" "path")
  (#eq? @_mk "method"))
