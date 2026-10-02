import nats


async def main():
    nc = await nats.connect("nats://localhost:4222")
    await nc.publish("label.printed", b"1")
    await nc.subscribe("label.jammed", cb=None)
