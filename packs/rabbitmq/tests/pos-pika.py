import pika


def send(channel: pika.adapters.blocking_connection.BlockingChannel, body: bytes):
    channel.basic_publish(exchange="", routing_key="dock.scheduled", body=body)


def listen(channel, callback):
    channel.basic_consume(queue="dock.cancelled", on_message_callback=callback)
