import functools


def cached(path):
    def wrap(fn):
        return functools.lru_cache(maxsize=32)(fn)

    return wrap


@cached("/v1/not-a-route")
def compute(value):
    return value * 2


settings = {"route": "/v1/also-not-a-route"}
