; stub = route_pb2_grpc.RouteServiceStub(channel) / self.stub = RouteServiceStub(channel)

(assignment
  left: [(identifier) (attribute)] @name
  right: (call
    function: [(attribute attribute: (identifier) @ctor) (identifier) @ctor])
  (#match? @ctor "Stub$"))
