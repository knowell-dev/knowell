; [Route("api/[controller]")] on a controller class.

(class_declaration
  (attribute_list
    (attribute
      name: (identifier) @_route
      (attribute_argument_list . (attribute_argument (string_literal) @prefix))))
  name: (identifier) @class
  (#eq? @_route "Route"))
