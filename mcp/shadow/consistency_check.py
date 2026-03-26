"""Shadow dual-write consistency checks and cutover gate evaluation."""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

if __package__ in (None, ""):
    sys.path.append(str(Path(__file__).resolve().parents[1]))

import store

DEFAULT_WORKERS = ("extractor", "causal", "consolidator", "distiller")
DEFAULT_SOURCES = ("extraction", "causal", "consolidation", "distillation")


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _iso_z(ts: datetime, timespec: str = "seconds") -> str:
    return ts.isoformat(timespec=timespec).replace("+00:00", "Z")


def _table_exists(conn: sqlite3.Connection, table: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
        (table,),
    ).fetchone()
    return row is not None


def _normalize_status(status: str) -> str | None:
    up = status.upper()
    if up == "DONE":
        return "done"
    if up == "FAILED":
        return "failed"
    if up in {"PENDING", "PROCESSING"}:
        return "pending"
    return None


def _empty_worker_counters() -> dict[str, dict[str, int]]:
    return {w: {"done": 0, "failed": 0, "pending": 0} for w in DEFAULT_WORKERS}


def _events_stats(conn: sqlite3.Connection, start_ts: str) -> dict[str, Any]:
    if not _table_exists(conn, "memory_events"):
        return {"total": 0, "by_worker": _empty_worker_counters(), "failure_rate": 0.0}

    total_row = conn.execute(
        "SELECT COUNT(*) AS cnt FROM memory_events WHERE created_at >= ?",
        (start_ts,),
    ).fetchone()
    total = int(total_row["cnt"] if total_row else 0)

    by_worker = _empty_worker_counters()
    rows = conn.execute(
        """
        SELECT worker_type, status, COUNT(*) AS cnt
        FROM memory_events
        WHERE created_at >= ?
        GROUP BY worker_type, status
        """,
        (start_ts,),
    ).fetchall()
    for row in rows:
        worker = str(row["worker_type"] or "unknown")
        bucket = _normalize_status(str(row["status"] or ""))
        if not bucket:
            continue
        if worker not in by_worker:
            by_worker[worker] = {"done": 0, "failed": 0, "pending": 0}
        by_worker[worker][bucket] += int(row["cnt"])

    done_24h = sum(item["done"] for item in by_worker.values())
    failed_24h = sum(item["failed"] for item in by_worker.values())
    denom_24h = done_24h + failed_24h
    failure_rate_24h = (failed_24h / denom_24h) if denom_24h else 0.0

    return {"total": total, "by_worker": by_worker, "failure_rate": failure_rate_24h}


def _failure_rate(conn: sqlite3.Connection, start_ts: str) -> tuple[float, int]:
    if not _table_exists(conn, "memory_events"):
        return 0.0, 0

    row = conn.execute(
        """
        SELECT
            SUM(CASE WHEN status = 'DONE' THEN 1 ELSE 0 END) AS done_cnt,
            SUM(CASE WHEN status = 'FAILED' THEN 1 ELSE 0 END) AS failed_cnt
        FROM memory_events
        WHERE created_at >= ?
        """,
        (start_ts,),
    ).fetchone()
    done_cnt = int(row["done_cnt"] or 0) if row else 0
    failed_cnt = int(row["failed_cnt"] or 0) if row else 0
    denom = done_cnt + failed_cnt
    if denom <= 0:
        return 0.0, 0
    return failed_cnt / denom, denom


def _memories_stats(conn: sqlite3.Connection) -> dict[str, Any]:
    if not _table_exists(conn, "memories"):
        return {
            "total": 0,
            "active": 0,
            "archived": 0,
            "by_source": {source: 0 for source in DEFAULT_SOURCES},
            "dedup_rate": 0.0,
        }

    total_row = conn.execute("SELECT COUNT(*) AS cnt FROM memories").fetchone()
    total = int(total_row["cnt"] if total_row else 0)

    active_row = conn.execute("SELECT COUNT(*) AS cnt FROM memories WHERE archived = 0").fetchone()
    active = int(active_row["cnt"] if active_row else 0)
    archived = max(total - active, 0)

    by_source = {source: 0 for source in DEFAULT_SOURCES}
    source_rows = conn.execute(
        """
        SELECT source, COUNT(*) AS cnt
        FROM memories
        GROUP BY source
        """
    ).fetchall()
    for row in source_rows:
        source = str(row["source"] or "unknown")
        if source not in by_source:
            by_source[source] = 0
        by_source[source] += int(row["cnt"])

    dedup_rate = (archived / total) if total else 0.0
    return {
        "total": total,
        "active": active,
        "archived": archived,
        "by_source": by_source,
        "dedup_rate": dedup_rate,
    }


def build_consistency_report(period_hours: int = 24, db_path: str | None = None) -> dict[str, Any]:
    period_hours = max(int(period_hours), 1)
    target_db = db_path or store.DB_PATH
    now = _utc_now()
    period_start = _iso_z(now - timedelta(hours=period_hours), timespec="milliseconds")
    failure_start = _iso_z(now - timedelta(hours=48), timespec="milliseconds")

    conn = store._sqlite_connect(target_db)
    try:
        events = _events_stats(conn, period_start)
        memories = _memories_stats(conn)
        failure_rate, completed_events_48h = _failure_rate(conn, failure_start)
    finally:
        conn.close()

    failure_rate_ok = completed_events_48h > 0 and failure_rate < 0.001
    dedup_rate = float(memories["dedup_rate"])
    dedup_rate_ok = 0.30 <= dedup_rate <= 0.50
    ready_for_cutover = failure_rate_ok and dedup_rate_ok

    return {
        "timestamp": _iso_z(now, timespec="seconds"),
        "period_hours": period_hours,
        "events": events,
        "memories": {
            "total": memories["total"],
            "active": memories["active"],
            "archived": memories["archived"],
            "by_source": memories["by_source"],
            "dedup_rate": dedup_rate,
        },
        "cutover_gates": {
            "failure_rate_ok": failure_rate_ok,
            "failure_rate": failure_rate,
            "dedup_rate_ok": dedup_rate_ok,
            "dedup_rate": dedup_rate,
        },
        "ready_for_cutover": ready_for_cutover,
    }


def build_pipeline_health(period_hours: int = 24, db_path: str | None = None) -> dict[str, Any]:
    report = build_consistency_report(period_hours=period_hours, db_path=db_path)
    by_worker = report["events"]["by_worker"]
    pending_total = sum(int(item.get("pending", 0)) for item in by_worker.values())
    return {
        "timestamp": report["timestamp"],
        "period_hours": report["period_hours"],
        "events_total": report["events"]["total"],
        "pending_total": pending_total,
        "by_worker": by_worker,
        "failure_rate_48h": report["cutover_gates"]["failure_rate"],
        "failure_rate_ok": report["cutover_gates"]["failure_rate_ok"],
        "dedup_rate": report["cutover_gates"]["dedup_rate"],
        "dedup_rate_ok": report["cutover_gates"]["dedup_rate_ok"],
        "ready_for_cutover": report["ready_for_cutover"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Check shadow dual-write consistency and cutover gates.")
    parser.add_argument("--db-path", default=store.DB_PATH, help="SQLite database path.")
    parser.add_argument("--period-hours", type=int, default=24, help="Event stats window in hours.")
    args = parser.parse_args()

    report = build_consistency_report(period_hours=args.period_hours, db_path=args.db_path)
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
