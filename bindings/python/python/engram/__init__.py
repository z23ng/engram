"""Engram (`mem`) Python SDK — Embedded PyO3 engine and HTTP/SSE daemon client."""

from .client import EngramClient, EngramError
from .local import Engram, open, open_in_memory

__all__ = [
    "Engram",
    "EngramClient",
    "EngramError",
    "open",
    "open_in_memory",
]
__version__ = "0.2.0"
