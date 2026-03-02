"""Shadow dual-write helpers for gradual cutover validation."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path
from typing import Any

if __package__ in (None, ""):
    sys.path.append(str(Path(__file__).resolve().parents[1]))

import store
from event_queue import enqueue, init_event_queue


def init_shadow_events_table(db_path: str) -> None:
    conn = store._sqlite_connect(db_path)
    try:
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS shadow_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL,
                conversation_id TEXT,
                turn_id TEXT,
                source TEXT DEFAULT 'dual_write',
                old_bridge_status TEXT DEFAULT 'UNKNOWN',
                new_pipeline_status TEXT DEFAULT 'PENDING',
                created_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            """
        )
    finally:
        conn.close()


def _log_shadow_event(
    db_path: str,
    event_id: str,
    conversation_id: str,
    turn_id: str,
    source: str = "dual_write",
    old_bridge_status: str = "UNKNOWN",
    new_pipeline_status: str = "PENDING",
) -> None:
    conn = store._sqlite_connect(db_path)
    try:
        conn.execute(
            """
            INSERT INTO shadow_events(
                event_id, conversation_id, turn_id, source, old_bridge_status, new_pipeline_status
            )
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (event_id, conversation_id, turn_id, source, old_bridge_status, new_pipeline_status),
        )
    finally:
        conn.close()


def shadow_ingest(
    db_path: str,
    conversation_id: str,
    turn_id: str,
    messages: list[Any],
) -> dict[str, Any]:
    conversation_id = str(conversation_id).strip()
    turn_id = str(turn_id).strip()
    if not conversation_id:
        raise ValueError("conversation_id is required")
    if not turn_id:
        raise ValueError("turn_id is required")
    if not isinstance(messages, list):
        raise ValueError("messages must be a list")

    event_id = hashlib.sha256(f"{conversation_id}{turn_id}".encode("utf-8")).hexdigest()
    payload = {
        "event_id": event_id,
        "conversation_id": conversation_id,
        "turn_id": turn_id,
        "messages": messages,
    }

    init_event_queue(db_path)
    init_shadow_events_table(db_path)

    extractor_enqueued = enqueue(db_path, event_id, "extractor", payload)
    causal_enqueued = enqueue(db_path, event_id, "causal", payload)
    _log_shadow_event(db_path, event_id, conversation_id, turn_id)

    return {
        "event_id": event_id,
        "enqueued": {
            "extractor": extractor_enqueued,
            "causal": causal_enqueued,
        },
        "shadow_logged": True,
    }
