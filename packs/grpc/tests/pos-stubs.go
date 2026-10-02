package fleet

import (
	"context"

	fleetv1 "example.com/gen/go/fleet/v1"
	"google.golang.org/grpc"
)

type server struct {
	fleetv1.UnimplementedRouteServiceServer
}

func (s *server) GetRoute(ctx context.Context, req *fleetv1.GetRouteRequest) (*fleetv1.Route, error) {
	return &fleetv1.Route{Id: req.RouteId}, nil
}

func Register(g *grpc.Server) {
	fleetv1.RegisterRouteServiceServer(g, &server{})
}

func Lookup(ctx context.Context, conn *grpc.ClientConn) (*fleetv1.Route, error) {
	client := fleetv1.NewRouteServiceClient(conn)
	return client.GetRoute(ctx, &fleetv1.GetRouteRequest{RouteId: "r1"})
}
