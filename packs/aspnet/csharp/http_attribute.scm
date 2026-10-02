; [HttpGet("{id}")] / [HttpPost] on a method of a class.

(class_declaration
  name: (identifier) @class
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute
          name: (identifier) @verb
          (attribute_argument_list . (attribute_argument (string_literal) @path))) @attribute)
      name: (identifier) @handler))
  (#any-of? @verb "HttpGet" "HttpPost" "HttpPut" "HttpPatch" "HttpDelete" "HttpHead" "HttpOptions"))

(class_declaration
  name: (identifier) @class
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute name: (identifier) @verb) @attribute)
      name: (identifier) @handler))
  (#any-of? @verb "HttpGet" "HttpPost" "HttpPut" "HttpPatch" "HttpDelete" "HttpHead" "HttpOptions"))
