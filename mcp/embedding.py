"""
Voyage-4 embedding client for Antigravity Memory MCP.
"""

import os
import httpx

VOYAGE_API_KEY = os.environ.get("VOYAGE_API_KEY", "")
VOYAGE_BASE_URL = "https://api.voyageai.com/v1"
VOYAGE_MODEL = "voyage-4"
VECTOR_DIM = 1024


async def embed(
    text: str,
    input_type: str = "document",
) -> list[float]:
    """
    Embed a single text using Voyage-4.
    input_type: "document" for storage, "query" for search.
    """
    if not VOYAGE_API_KEY:
        raise ValueError("VOYAGE_API_KEY not set")

    async with httpx.AsyncClient(timeout=15) as client:
        r = await client.post(
            f"{VOYAGE_BASE_URL}/embeddings",
            headers={
                "Authorization": f"Bearer {VOYAGE_API_KEY}",
                "Content-Type": "application/json",
            },
            json={
                "model": VOYAGE_MODEL,
                "input": [text],
                "input_type": input_type,
            },
        )
        r.raise_for_status()
        data = r.json()
        vec = data["data"][0]["embedding"]
        if len(vec) != VECTOR_DIM:
            raise ValueError(f"Expected {VECTOR_DIM}d, got {len(vec)}d")
        return vec


async def embed_batch(
    texts: list[str],
    input_type: str = "document",
) -> list[list[float]]:
    """Embed multiple texts in one API call (max 128)."""
    if not VOYAGE_API_KEY:
        raise ValueError("VOYAGE_API_KEY not set")
    if not texts:
        return []

    async with httpx.AsyncClient(timeout=30) as client:
        r = await client.post(
            f"{VOYAGE_BASE_URL}/embeddings",
            headers={
                "Authorization": f"Bearer {VOYAGE_API_KEY}",
                "Content-Type": "application/json",
            },
            json={
                "model": VOYAGE_MODEL,
                "input": texts[:128],
                "input_type": input_type,
            },
        )
        r.raise_for_status()
        data = r.json()
        return [d["embedding"] for d in data["data"]]
