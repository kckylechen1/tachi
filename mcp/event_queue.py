"""SQLite-backed async worker queue for memory events."""

from __future__ import annotations

import json
import os
import sqlite3
from datetime import datetime, timedelta, timezone
from typing import Any


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _utc_after(seconds: int) -> str:
    return (datetime.now(timezone.utc) + timedelta(seconds=seconds)).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _connect(db_path: str) -> sqlite3.Connection:
    os.makedirs(os.path.dirname(db_path), exist_ok=True)
    conn = sqlite3.connect(db_path, timeout=30, isolation_level=None)
    conn.row_factory = sqlite3.Row
    return conn


def init_event_queue(db_path: str) -> None:
    conn = _connect(db_path)
    try:
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS memory_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL,
                worker_type TEXT NOT NULL,
                status TEXT DEFAULT 'PENDING',
                payload TEXT NOT NULL,
                retry_count INTEGER DEFAULT 0,
                locked_until TEXT,
                last_error TEXT,
                created_at TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                processed_at TEXT
            );
            """
        )
        # Keep idempotency per (event_id, worker_type).
        conn.execute(
            """
            CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_events_event_worker
            ON memory_events(event_id, worker_type);
            """
        )
    finally:
        conn.close()


def enqueue(db_path: str, event_id: str, worker_type: str, payload: dict[str, Any] | str) -> bool:
    init_event_queue(db_path)
    payload_text = payload if isinstance(payload, str) else json.dumps(payload, ensure_ascii=False)

    conn = _connect(db_path)
    try:
        cur = conn.execute(
            """
            INSERT OR IGNORE INTO memory_events(event_id, worker_type, status, payload)
            VALUES (?, ?, 'PENDING', ?)
            """,
            (event_id, worker_type, payload_text),
        )
        return cur.rowcount > 0
    finally:
        conn.close()


def claim(db_path: str, worker_type: str, lock_seconds: int = 60) -> dict[str, Any] | None:
    init_event_queue(db_path)
    now = _utc_now()
    lock_until = _utc_after(lock_seconds)

    conn = _connect(db_path)
    transaction_started = False
    try:
        conn.execute("BEGIN IMMEDIATE")
        transaction_started = True
        row = conn.execute(
            """
            SELECT id, event_id, worker_type, status, payload,
                   retry_count, locked_until, last_error, created_at, processed_at
            FROM memory_events
            WHERE worker_type = ?
              AND status IN ('PENDING', 'FAILED')
              AND (locked_until IS NULL OR locked_until <= ?)
            ORDER BY created_at ASC, id ASC
            LIMIT 1
            """,
            (worker_type, now),
        ).fetchone()

        if row is None:
            conn.execute("COMMIT")
            return None

        conn.execute(
            """
            UPDATE memory_events
            SET status = 'PROCESSING', locked_until = ?, last_error = NULL
            WHERE id = ?
            """,
            (lock_until, row["id"]),
        )
        conn.execute("COMMIT")

        task = dict(row)
        task["status"] = "PROCESSING"
        task["locked_until"] = lock_until
        return task
    except Exception:
        if transaction_started:
            conn.execute("ROLLBACK")
        raise
    finally:
        conn.close()


def complete(db_path: str, event_id: str, worker_type: str) -> None:
    conn = _connect(db_path)
    try:
        conn.execute(
            """
            UPDATE memory_events
            SET status = 'DONE',
                processed_at = ?,
                locked_until = NULL,
                last_error = NULL
            WHERE event_id = ? AND worker_type = ?
            """,
            (_utc_now(), event_id, worker_type),
        )
    finally:
        conn.close()


def fail(db_path: str, event_id: str, worker_type: str, error: str) -> None:
    conn = _connect(db_path)
    try:
        conn.execute(
            """
            UPDATE memory_events
            SET status = 'FAILED',
                retry_count = retry_count + 1,
                last_error = ?,
                locked_until = NULL,
                processed_at = ?
            WHERE event_id = ? AND worker_type = ?
            """,
            (error, _utc_now(), event_id, worker_type),
        )
    finally:
        conn.close()


def get_pending_count(db_path: str, worker_type: str) -> int:
    init_event_queue(db_path)
    conn = _connect(db_path)
    try:
        row = conn.execute(
            """
            SELECT COUNT(*) AS cnt
            FROM memory_events
            WHERE worker_type = ? AND status = 'PENDING'
            """,
            (worker_type,),
        ).fetchone()
        return int(row["cnt"] if row else 0)
    finally:
        conn.close()
