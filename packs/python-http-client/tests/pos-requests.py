import requests

BASE = "https://api.example.com"


def get_route(route_id):
    return requests.get(f"{BASE}/v1/routes/{route_id}", timeout=5)


def close_route(route_id):
    return requests.post(BASE + "/v1/routes/" + route_id + "/close")
