import Redis from "ioredis";

const pub = new Redis();
const sub = new Redis();

export async function wire() {
  await sub.subscribe("cache.invalidated");
  await pub.publish("cache.warmed", "1");
}
