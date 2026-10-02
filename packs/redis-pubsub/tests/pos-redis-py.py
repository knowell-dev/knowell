import redis

r = redis.Redis()


def wire():
    r.publish("aisle.closed", "1")
    p = r.pubsub()
    p.subscribe("aisle.opened")
