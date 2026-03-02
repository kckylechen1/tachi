"""Simple retrieval latency benchmark for shadow cutover gates."""

from __future__ import annotations

import argparse
import json
import math
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from memory_core_py import MemoryStore

if __package__ in (None, ""):
    sys.path.append(str(Path(__file__).resolve().parents[1]))

import store


def _iso_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = (len(ordered) - 1) * q
    lo = int(math.floor(idx))
    hi = int(math.ceil(idx))
    if lo == hi:
        return ordered[lo]
    weight = idx - lo
    return ordered[lo] * (1.0 - weight) + ordered[hi] * weight


def _table_exists(db_path: str, table: str) -> bool:
    conn = store._sqlite_connect(db_path)
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            (table,),
        ).fetchone()
        return row is not None
    finally:
        conn.close()


def _sample_queries(db_path: str, sample_size: int) -> list[str]:
    if not _table_exists(db_path, "memories"):
        return []

    conn = store._sqlite_connect(db_path)
    try:
        rows = conn.execute(
            """
            SELECT text
            FROM memories
            WHERE archived = 0
              AND text IS NOT NULL
              AND TRIM(text) != ''
            ORDER BY RANDOM()
            LIMIT ?
            """,
            (sample_size,),
        ).fetchall()
        return [str(row["text"]) for row in rows]
    finally:
        conn.close()


def run_benchmark(db_path: str | None = None, sample_size: int = 10, top_k: int = 6) -> dict:
    target_db = db_path or store.DB_PATH
    sample_size = max(int(sample_size), 1)
    top_k = max(int(top_k), 1)

    queries = _sample_queries(target_db, sample_size)
    latencies_ms: list[float] = []

    memory_store = MemoryStore(target_db)
    for text in queries:
        start = time.perf_counter()
        store.search_by_text(
            memory_store,
            query=text,
            top_k=top_k,
            include_archived=False,
        )
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        latencies_ms.append(elapsed_ms)

    p50 = _percentile(latencies_ms, 0.50)
    p95 = _percentile(latencies_ms, 0.95)
    p99 = _percentile(latencies_ms, 0.99)
    p95_ok = bool(latencies_ms) and p95 < 200.0

    return {
        "timestamp": _iso_now(),
        "sample_size": len(latencies_ms),
        "target_sample_size": sample_size,
        "top_k": top_k,
        "latency_ms": {
            "p50": p50,
            "p95": p95,
            "p99": p99,
            "samples": latencies_ms,
        },
        "cutover_gates": {
            "p95_ok": p95_ok,
            "p95_threshold_ms": 200.0,
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Run simple memory retrieval latency benchmark.")
    parser.add_argument("--db-path", default=store.DB_PATH, help="SQLite database path.")
    parser.add_argument("--sample-size", type=int, default=10, help="Number of random memories to sample.")
    parser.add_argument("--top-k", type=int, default=6, help="top_k used in search_by_text.")
    args = parser.parse_args()

    report = run_benchmark(db_path=args.db_path, sample_size=args.sample_size, top_k=args.top_k)
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
