import httpx


async def list_stops(client: httpx.AsyncClient):
    response = await client.get("/v1/stops")
    return response.json()


def delete_stop(stop_id: str):
    with httpx.Client(base_url="https://api.example.com") as http:
        return http.delete(f"/v1/stops/{stop_id}")
