import amqp from "amqplib";

export async function setup() {
  const connection = await amqp.connect("amqp://localhost");
  const channel = await connection.createChannel();
  await channel.assertQueue("pallet.inspections");
  channel.sendToQueue("pallet.inspections", Buffer.from("{}"));
  channel.publish("warehouse", "pallet.damaged", Buffer.from("{}"));
  await channel.consume("pallet.repairs", (msg) => msg);
}
