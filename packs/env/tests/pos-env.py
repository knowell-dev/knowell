import os

from pydantic_settings import BaseSettings

TIMEOUT = os.environ.get("GATE_TIMEOUT", "5")
TOKEN_FILE = os.environ["GATE_TOKEN_FILE"]
HOST = os.getenv("GATE_HOST")


class Settings(BaseSettings):
    gate_region: str = "eu"
    retries: int = 3
