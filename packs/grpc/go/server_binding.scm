; type server struct { pb.UnimplementedRouteServiceServer }

(type_spec
  name: (type_identifier) @name
  type: (struct_type
    (field_declaration_list
      (field_declaration
        type: [(qualified_type name: (type_identifier) @embedded) (type_identifier) @embedded])))
  (#match? @embedded "^Unimplemented.+Server$"))
