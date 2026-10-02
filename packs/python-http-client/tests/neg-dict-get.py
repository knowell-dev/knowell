cache = {}


def lookup(path):
    return cache.get("/v1/stops") or path
