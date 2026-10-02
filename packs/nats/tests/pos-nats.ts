import { connect, StringCodec } from "nats";

export async function run() {
  const nc = await connect({ servers: "localhost:4222" });
  const sc = StringCodec();
  nc.publish("scanner.read", sc.encode("1"));
  const sub = nc.subscribe("scanner.error");
  return sub;
}
