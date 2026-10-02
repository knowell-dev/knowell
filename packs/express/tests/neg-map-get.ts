import express from "express";

const cache = new Map<string, string>();

export function lookup(key: string): string | undefined {
  // `get` with one argument is a lookup, not a route registration.
  cache.get("/v1/not-a-route");
  return cache.get(key);
}

export const app = express();
app.set("trust proxy", true);
