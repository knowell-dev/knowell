import { EventEmitter } from "node:events";

const bus = new EventEmitter();

export function notify(listener: () => void) {
  bus.on("refresh", listener);
  bus.emit("refresh");
  return new Map([["topic", "not.a.topic"]]);
}
