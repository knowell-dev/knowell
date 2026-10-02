import Fastify from "fastify";

export function buildServer() {
  const app = Fastify();
  app.get("/v2/sprockets/:id", async (request) => ({ id: request.params }));
  app.route({ method: "PUT", url: "/v2/sprockets/:id", handler: async () => ({}) });
  return app;
}
