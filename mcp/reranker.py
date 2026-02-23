"""
Voyage rerank-2.5 client for Antigravity Memory MCP.
Reranks search results after hybrid retrieval to improve precision.
"""

import os
import logging
import httpx

logger = logging.getLogger("memory-mcp")

VOYAGE_API_KEY = os.environ.get("VOYAGE_API_KEY", "")
VOYAGE_BASE_URL = "https://api.voyageai.com/v1"
RERANK_MODEL = "rerank-2.5"


async def rerank(
    query: str,
    results: list[dict],
    top_k: int = 6,
) -> list[dict]:
    """
    Rerank search results using Voyage rerank-2.5.
    Falls back to original order on any failure.

    Args:
        query: The search query text.
        results: List of memory dicts from hybrid_search (must have 'text' key).
        top_k: Number of top results to return after reranking.

    Returns:
        Reranked list of memory dicts, trimmed to top_k.
    """
    if not results:
        return results

    if not VOYAGE_API_KEY:
        logger.warning("VOYAGE_API_KEY not set, skipping rerank")
        return results[:top_k]

    documents = [r["text"] for r in results]

    try:
        async with httpx.AsyncClient(timeout=10) as client:
            resp = await client.post(
                f"{VOYAGE_BASE_URL}/rerank",
                headers={
                    "Authorization": f"Bearer {VOYAGE_API_KEY}",
                    "Content-Type": "application/json",
                },
                json={
                    "model": RERANK_MODEL,
                    "query": query,
                    "documents": documents,
                    "top_k": top_k,
                },
            )
            resp.raise_for_status()
            data = resp.json()

        # Rebuild results in reranked order, injecting relevance_score
        reranked = []
        for item in data["data"]:
            idx = item["index"]
            entry = {**results[idx]}
            entry["rerank_score"] = round(item["relevance_score"], 4)
            reranked.append(entry)

        logger.info(f"Reranked {len(results)} → top {len(reranked)} results")
        return reranked

    except Exception as e:
        logger.warning(f"Rerank failed, falling back to original order: {e}")
        return results[:top_k]
