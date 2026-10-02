import * as grpc from "@grpc/grpc-js";
import { RouteServiceClient, RouteServiceService } from "./gen/route_grpc_pb";

export function serve(server: grpc.Server) {
  server.addService(RouteServiceService, {
    getRoute: (call: unknown, callback: (e: null, r: object) => void) => callback(null, {}),
  });
}

export function lookup(address: string) {
  const client = new RouteServiceClient(address, grpc.credentials.createInsecure());
  client.getRoute({ routeId: "r1" }, () => undefined);
}
