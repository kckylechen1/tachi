#!/usr/bin/env python3
"""One-shot script: enqueue existing memories into the consolidator queue."""

import os
import sys
import sqlite3

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "mcp"))

from event_queue import enqueue, init_event_queue

DB_PATH = os.environ.get("MEMORY_DB_PATH", os.path.expanduser("~/.gemini/antigravity/memory.db"))


def main():
    init_event_queue(DB_PATH)
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT id, path FROM memories WHERE archived = 0"
    ).fetchall()
    conn.close()

    enqueued = 0
    for row in rows:
        mid = row["id"]
        path = row["path"] or "/"
        ok = enqueue(
            DB_PATH,
            f"backfill:consolidator:{mid}",
            "consolidator",
            {"memory_id": mid, "path": path, "origin": "backfill"},
        )
        if ok:
            enqueued += 1

    print(f"Done. Enqueued {enqueued}/{len(rows)} memories for consolidation.")


if __name__ == "__main__":
    main()
