from aiokafka import AIOKafkaConsumer, AIOKafkaProducer

BIN_EMPTIED = "bin.emptied"


async def publish(producer: AIOKafkaProducer, bin_id: str):
    await producer.send_and_wait(BIN_EMPTIED, bin_id.encode())


def consumer() -> AIOKafkaConsumer:
    return AIOKafkaConsumer("bin.filled", "bin.moved", group_id="bins")
