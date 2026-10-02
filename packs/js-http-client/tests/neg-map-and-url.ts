export function track(event: string) {
  // Neither a Map lookup nor a URL constructor is an HTTP call.
  const seen = new Map<string, number>();
  seen.get("/v1/events");
  window.location.assign(`/orders/${event}`);
  return new URL("/v1/events", "https://example.com").toString();
}
