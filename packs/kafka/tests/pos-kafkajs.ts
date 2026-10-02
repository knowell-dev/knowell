import { Kafka } from "kafkajs";

const kafka = new Kafka({ clientId: "shipments", brokers: ["localhost:9092"] });
const producer = kafka.producer();
const consumer = kafka.consumer({ groupId: "shipments" });

export const SHIPMENT_DISPATCHED = "shipment.dispatched";

export async function announce(id: string) {
  await producer.send({ topic: SHIPMENT_DISPATCHED, messages: [{ key: id, value: id }] });
}

export async function listen() {
  await consumer.subscribe({ topics: ["shipment.returned", "shipment.lost"], fromBeginning: false });
  await consumer.subscribe({ topic: "shipment.delayed" });
}

export class Outbox {
  constructor(private readonly events: { publish(topic: string, data: unknown): Promise<void> }) {}

  async flush(topic: string, data: unknown) {
    await this.events.publish(SHIPMENT_DISPATCHED, data);
    await producer.send({ topic, messages: [] });
  }
}
