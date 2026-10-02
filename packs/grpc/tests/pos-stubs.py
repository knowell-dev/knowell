import grpc

from example.fleet.v1 import route_pb2, route_pb2_grpc


class RouteServicer(route_pb2_grpc.RouteServiceServicer):
    def GetRoute(self, request, context):
        return route_pb2.Route(id=request.route_id)


def lookup(channel: grpc.Channel, route_id: str):
    stub = route_pb2_grpc.RouteServiceStub(channel)
    return stub.GetRoute(route_pb2.GetRouteRequest(route_id=route_id))
