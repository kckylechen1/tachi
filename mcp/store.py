"""
SQLite + sqlite-vec storage layer for Antigravity Memory MCP.
Now powered by Rust-based memory-core-py.

Dedup strategy (based on research):
  >= 0.96  → hard skip (near-exact duplicate, A-Mem / arXiv 2602.00959)
  0.75-0.96 → evolve: upsert with source='update' (A-Mem NeurIPS 2025)
  < 0.75   → insert as new memory

Ref:
  - A-Mem: Agentic Memory for LLM Agents (NeurIPS 2025)
    https://openreview.net/forum?id=FiM0M8gcct
  - Probing Knowledge Boundary (arXiv 2602.00959, 2025)
    https://arxiv.org/abs/2602.00959
  - Synapse: Episodic-Semantic Memory for LLM Agents (arXiv 2601.02744)
    https://arxiv.org/abs/2601.02744
"""

import json
import os
import sqlite3
import struct
import uuid
from datetime import datetime, timezone
from typing import Any
from memory_core_py import MemoryStore
import config as mcp_config

DB_PATH = mcp_config.DB_PATH

# ── Dedup thresholds ─────────────────────────────────────────────────────────
# Inspired by A-Mem (NeurIPS 2025) and arXiv:2602.00959:
#   cosine >= HARD_SKIP  → true duplicate, silently drop
#   cosine >= EVOLVE     → semantically related update, overwrite existing
#   cosine <  EVOLVE     → new knowledge, insert fresh
HARD_SKIP_THRESHOLD = float(os.environ.get("MEMORY_DEDUP_HARD_SKIP", "0.96"))
EVOLVE_THRESHOLD    = float(os.environ.get("MEMORY_DEDUP_EVOLVE",    "0.75"))
# ─────────────────────────────────────────────────────────────────────────────


def get_connection() -> MemoryStore:
    """Get a database connection powered by the Rust memory-core backend."""
    os.makedirs(os.path.dirname(DB_PATH), exist_ok=True)
    return MemoryStore(DB_PATH)

def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def save_memory(
    store: MemoryStore,
    text: str,
    vector: list[float],
    path: str = "/",
    summary: str = "",
    topic: str = "",
    keywords: list[str] | None = None,
    scope: str = "general",
    importance: float = 0.7,
    source: str = "manual",
    metadata: dict[str, Any] | None = None,
) -> dict | None:
    """
    Save a memory entry with two-stage dedup (A-Mem / arXiv:2602.00959).

    Returns:
      dict   → saved/evolved successfully
      None   → hard-skip (near-exact duplicate, cosine >= HARD_SKIP_THRESHOLD)
    """
    entry_id = str(uuid.uuid4())
    now = _utc_now()

    # ── Stage 1: vector similarity check ─────────────────────────────────────
    top_score: float = 0.0
    top_existing_id: str | None = None
    try:
        dedup_opts = json.dumps({
            "top_k": 1,
            "query_vec": vector,
            "weights": {
                "semantic": 1.0,
                "fts": 0.0,
                "symbolic": 0.0,
                "decay": 0.0,
            },
        })
        res_str = store.search(text, dedup_opts)
        results = json.loads(res_str)
        if results:
            top_score = results[0].get("score", {}).get("final", 0.0)
            top_existing_id = results[0].get("entry", {}).get("id")
    except Exception:
        # Empty DB or vec0 not ready — skip dedup entirely, proceed to insert
        pass

    # ── Stage 2: threshold routing ────────────────────────────────────────────
    # >= HARD_SKIP (0.96): near-exact duplicate → drop silently
    if top_score >= HARD_SKIP_THRESHOLD:
        return None

    # Build the memory entry payload
    me = {
        "id": entry_id,
        "path": path,
        "summary": summary,
        "text": text,
        "importance": importance,
        "timestamp": now,
        "category": "fact",
        "topic": topic,
        "keywords": keywords or [],
        "persons": [],
        "entities": [],
        "location": "",
        "source": source,
        "scope": scope,
        "vector": vector,
        "metadata": metadata or {},
    }

    # EVOLVE_THRESHOLD (0.75) <= score < HARD_SKIP (0.96):
    # Semantically related but not identical — treat as an evolution/update.
    # Per A-Mem (NeurIPS 2025): memories should evolve, not be silently dropped.
    # We reuse the existing ID so the entry is overwritten in-place (upsert).
    if top_score >= EVOLVE_THRESHOLD and top_existing_id:
        me["id"] = top_existing_id
        me["source"] = "update"

    # < EVOLVE_THRESHOLD: genuinely new knowledge → insert with fresh UUID
    try:
        store.upsert(json.dumps(me))
        return {
            "id": me["id"],
            "text": text,
            "path": path,
            "summary": summary,
            "topic": topic,
            "created_at": now,
        }
    except Exception as e:
        print(f"Failed to upsert memory: {e}")
        raise


def get_memory(store: MemoryStore, entry_id: str, include_archived: bool = False) -> dict | None:
    """Retrieve full memory by ID."""
    res = store.get(entry_id, include_archived)
    if not res:
        return None
    try:
        data = json.loads(res)
        if "timestamp" in data:
            data["created_at"] = data["timestamp"]
        return data
    except (json.JSONDecodeError, TypeError) as e:
        print(f"Error parsing memory {entry_id}: {e}")
        return None


def search_by_vector(
    store: MemoryStore,
    query_vec: list[float],
    top_k: int = 8,
    path_prefix: str = "",
    include_archived: bool = False,
) -> list[dict]:
    """Fallback search by vector (pure vector search simulated by 0 lexical weights)."""
    opts = {
        "top_k": top_k,
        "query_vec": query_vec,
        "path_prefix": path_prefix,
        "include_archived": include_archived,
        "weights": {
            "semantic": 1.0,
            "fts": 0.0,
            "symbolic": 0.0,
            "decay": 0.0
        }
    }
    try:
        res_str = store.search("", json.dumps(opts))
        results = json.loads(res_str)
        legacy_results = []
        for r in results:
            entry = r["entry"]
            legacy_results.append({
                "id": entry["id"],
                "text": entry["text"],
                "path": entry["path"],
                "summary": entry["summary"],
                "topic": entry["topic"],
                "keywords": entry["keywords"],
                "scope": entry["scope"],
                "importance": entry["importance"],
                "created_at": entry["timestamp"],
                "score": r["score"]["final"],
            })
        return legacy_results
    except Exception:
        return []


def search_by_text(
    store: MemoryStore,
    query: str,
    top_k: int = 10,
    path_prefix: str = "",
    include_archived: bool = False,
) -> list[dict]:
    """Fallback search by text (pure FTS search)."""
    opts = {
        "top_k": top_k,
        "path_prefix": path_prefix,
        "include_archived": include_archived,
        "weights": {
            "semantic": 0.0,
            "fts": 1.0,
            "symbolic": 0.0,
            "decay": 0.0
        }
    }
    try:
        res_str = store.search(query, json.dumps(opts))
        results = json.loads(res_str)
        legacy_results = []
        for r in results:
            entry = r["entry"]
            legacy_results.append({
                "id": entry["id"],
                "text": entry["text"],
                "path": entry["path"],
                "summary": entry["summary"],
                "topic": entry["topic"],
                "keywords": entry["keywords"],
                "scope": entry["scope"],
                "importance": entry["importance"],
                "created_at": entry["timestamp"],
                "fts_rank": r["score"]["fts"],
                "score": r["score"]["final"],
            })
        return legacy_results
    except Exception:
        return []


def hybrid_search(
    store: MemoryStore,
    query_vec: list[float],
    query_text: str,
    top_k: int = 6,
    path_prefix: str = "",
    include_archived: bool = False,
    w_vec: float = 0.6,
    w_lex: float = 0.25,
    w_recency: float = 0.15,
) -> list[dict]:
    """Hybrid search delegated to Rust core."""
    opts = {
        "top_k": top_k,
        "query_vec": query_vec,
        "path_prefix": path_prefix,
        "include_archived": include_archived,
        "weights": {
            "semantic": w_vec,
            "fts": w_lex,
            "symbolic": 0.0,
            "decay": w_recency
        }
    }
    try:
        res_str = store.search(query_text, json.dumps(opts))
        results = json.loads(res_str)
        legacy_results = []
        for r in results:
            entry = r["entry"]
            legacy_results.append({
                "id": entry["id"],
                "text": entry["text"],
                "path": entry["path"],
                "summary": entry["summary"],
                "topic": entry["topic"],
                "keywords": entry["keywords"],
                "scope": entry["scope"],
                "importance": entry["importance"],
                "created_at": entry["timestamp"],
                "score": r["score"]["final"],
            })
        return legacy_results
    except Exception as e:
        import logging
        logging.getLogger(mcp_config.logger_name("store")).exception("Hybrid search failed")
        raise


def list_by_path(
    store: MemoryStore,
    path_prefix: str,
    include_archived: bool = False,
    limit: int = 5000,
) -> dict:
    """List sub-paths and memories immediately under a given path."""
    normalized_path = (path_prefix or "").strip() or "/"
    if not normalized_path.startswith('/'):
        normalized_path = '/' + normalized_path
    if len(normalized_path) > 1:
        normalized_path = normalized_path.rstrip('/')

    child_prefix = "/" if normalized_path == "/" else f"{normalized_path}/"

    try:
        entries = json.loads(store.list_by_path(normalized_path, limit, include_archived))
        dirs = set()
        memories = []
        for r in entries:
            p = r.get("path", "")
            if p == normalized_path:
                summary = r.get("summary") or (r.get("text", "")[:60] + "...")
                memories.append({
                    "id": r["id"],
                    "path": p,
                    "summary": summary,
                    "importance": r.get("importance", 0.7),
                    "created_at": r.get("timestamp", ""),
                })
                continue
            if not p.startswith(child_prefix):
                continue
            rel = p[len(child_prefix):]
            sub_dir = rel.split('/', 1)[0]
            if sub_dir:
                dirs.add(sub_dir)
        return {
            "path": normalized_path,
            "directories": sorted(list(dirs)),
            "memories": memories,
        }
    except Exception as e:
        print(f"list_by_path failed: {e}")
        return {"path": normalized_path, "directories": [], "memories": []}


def get_stats(store: MemoryStore, include_archived: bool = False) -> dict:
    """Get memory db statistics."""
    try:
        res_str = store.get_all(100000, include_archived)
        all_entries = json.loads(res_str)
        count = len(all_entries)
        scopes = {}
        root_paths = {}
        for r in all_entries:
            s = r.get("scope", "general")
            scopes[s] = scopes.get(s, 0) + 1
            p = r.get("path", "/")
            parts = p.split('/')
            if len(parts) > 1 and parts[1]:
                root_path = f"/{parts[1]}"
                root_paths[root_path] = root_paths.get(root_path, 0) + 1
        return {
            "total_memories": count,
            "root_paths": root_paths,
            "scopes": scopes,
            "db_path": DB_PATH,
        }
    except Exception:
        return {
            "total_memories": 0,
            "root_paths": {},
            "scopes": {},
            "db_path": DB_PATH,
        }


def _sqlite_connect(db_path: str | None = None) -> sqlite3.Connection:
    path = db_path or DB_PATH
    os.makedirs(os.path.dirname(path), exist_ok=True)
    conn = sqlite3.connect(path, timeout=30, isolation_level=None)
    conn.row_factory = sqlite3.Row
    return conn


def ensure_revision_column(db_path: str | None = None) -> None:
    conn = _sqlite_connect(db_path)
    try:
        cols = conn.execute("PRAGMA table_info(memories)").fetchall()
        names = {c["name"] for c in cols}
        if "revision" not in names:
            conn.execute("ALTER TABLE memories ADD COLUMN revision INTEGER NOT NULL DEFAULT 1")
    finally:
        conn.close()


def get_memory_row(db_path: str, entry_id: str, include_archived: bool = False) -> dict | None:
    ensure_revision_column(db_path)
    conn = _sqlite_connect(db_path)
    try:
        sql = """
            SELECT id, path, summary, text, topic, source, scope, importance,
                   archived, created_at, updated_at, metadata, revision, timestamp
            FROM memories
            WHERE id = ?
        """
        params: list[Any] = [entry_id]
        if not include_archived:
            sql += " AND archived = 0"
        row = conn.execute(sql, params).fetchone()
        if not row:
            return None
        out = dict(row)
        meta_raw = out.get("metadata") or "{}"
        try:
            out["metadata"] = json.loads(meta_raw) if isinstance(meta_raw, str) else meta_raw
        except json.JSONDecodeError:
            out["metadata"] = {}
        return out
    finally:
        conn.close()


def count_memories_by_source(
    db_path: str,
    source: str,
    path_prefix: str = "",
    include_archived: bool = False,
) -> int:
    conn = _sqlite_connect(db_path)
    try:
        sql = "SELECT COUNT(*) AS cnt FROM memories WHERE source = ?"
        params: list[Any] = [source]
        if path_prefix:
            normalized = path_prefix if path_prefix.startswith("/") else f"/{path_prefix}"
            normalized = normalized.rstrip("/") or "/"
            like_prefix = "/%" if normalized == "/" else f"{normalized}/%"
            sql += " AND (path = ? OR path LIKE ?)"
            params.extend([normalized, like_prefix])
        if not include_archived:
            sql += " AND archived = 0"
        row = conn.execute(sql, params).fetchone()
        return int(row["cnt"] if row else 0)
    finally:
        conn.close()


def list_memories_by_source(
    db_path: str,
    source: str,
    path_prefix: str = "",
    include_archived: bool = False,
    limit: int = 5000,
) -> list[dict]:
    conn = _sqlite_connect(db_path)
    try:
        sql = """
            SELECT id, path, summary, text, topic, source, scope, importance,
                   archived, created_at, updated_at, metadata, timestamp
            FROM memories
            WHERE source = ?
        """
        params: list[Any] = [source]
        if path_prefix:
            normalized = path_prefix if path_prefix.startswith("/") else f"/{path_prefix}"
            normalized = normalized.rstrip("/") or "/"
            like_prefix = "/%" if normalized == "/" else f"{normalized}/%"
            sql += " AND (path = ? OR path LIKE ?)"
            params.extend([normalized, like_prefix])
        if not include_archived:
            sql += " AND archived = 0"
        sql += " ORDER BY timestamp DESC LIMIT ?"
        params.append(limit)
        rows = conn.execute(sql, params).fetchall()
        out: list[dict] = []
        for row in rows:
            item = dict(row)
            meta_raw = item.get("metadata") or "{}"
            try:
                item["metadata"] = json.loads(meta_raw) if isinstance(meta_raw, str) else meta_raw
            except json.JSONDecodeError:
                item["metadata"] = {}
            out.append(item)
        return out
    finally:
        conn.close()


def merge_memory_with_revision(
    db_path: str,
    target_id: str,
    expected_revision: int,
    merged_text: str,
    merged_summary: str,
    merged_vector: list[float] | None = None,
    archive_id: str | None = None,
) -> bool:
    ensure_revision_column(db_path)
    now = _utc_now()
    conn = _sqlite_connect(db_path)
    try:
        conn.execute("BEGIN IMMEDIATE")
        row = conn.execute(
            """
            SELECT id, path, keywords, entities, revision
            FROM memories
            WHERE id = ? AND archived = 0
            """,
            (target_id,),
        ).fetchone()
        if row is None or int(row["revision"]) != int(expected_revision):
            conn.execute("ROLLBACK")
            return False

        cur = conn.execute(
            """
            UPDATE memories
            SET text = ?,
                summary = ?,
                source = 'consolidation',
                updated_at = ?,
                revision = revision + 1
            WHERE id = ? AND revision = ? AND archived = 0
            """,
            (merged_text, merged_summary, now, target_id, expected_revision),
        )
        if cur.rowcount != 1:
            conn.execute("ROLLBACK")
            return False

        keywords_json = row["keywords"] or "[]"
        entities_json = row["entities"] or "[]"
        try:
            keywords = " ".join(json.loads(keywords_json))
        except Exception:
            keywords = ""
        try:
            entities = " ".join(json.loads(entities_json))
        except Exception:
            entities = ""

        conn.execute("DELETE FROM memories_fts WHERE id = ?", (target_id,))
        conn.execute(
            """
            INSERT INTO memories_fts(id, path, summary, text, keywords, entities)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (target_id, row["path"], merged_summary, merged_text, keywords, entities),
        )

        if merged_vector:
            blob = struct.pack(f"{len(merged_vector)}f", *merged_vector)
            try:
                conn.execute("DELETE FROM memories_vec WHERE id = ?", (target_id,))
                conn.execute(
                    "INSERT INTO memories_vec(id, embedding) VALUES (?, ?)",
                    (target_id, blob),
                )
            except sqlite3.Error:
                pass

        if archive_id:
            conn.execute(
                """
                UPDATE memories
                SET archived = 1, updated_at = ?
                WHERE id = ?
                """,
                (now, archive_id),
            )

        conn.execute("COMMIT")
        return True
    except Exception:
        conn.execute("ROLLBACK")
        raise
    finally:
        conn.close()


# ── hard_state: deterministic KV store ─────────────────────────────────────────
# Phase 2: Structured state that should NEVER go through vector search.
# Schema: (namespace, key) → value_json, with version tracking.

def ensure_hard_state_table(db_path: str | None = None) -> None:
    """Create hard_state table if it doesn't exist."""
    conn = _sqlite_connect(db_path)
    try:
        conn.execute("""
            CREATE TABLE IF NOT EXISTS hard_state (
                namespace        TEXT NOT NULL,
                key              TEXT NOT NULL,
                value_json       TEXT NOT NULL DEFAULT '{}',
                version          INTEGER NOT NULL DEFAULT 1,
                expires_at       TEXT,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                last_modified_by TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (namespace, key)
            )
        """)
        conn.execute("""
            CREATE INDEX IF NOT EXISTS idx_hard_state_expires
            ON hard_state(namespace, expires_at)
        """)
    finally:
        conn.close()


def set_state(
    db_path: str | None = None,
    namespace: str = "default",
    key: str = "",
    value: Any = None,
    modified_by: str = "",
) -> dict:
    """Set a hard state value. INSERT OR UPDATE with version bump."""
    ensure_hard_state_table(db_path)
    conn = _sqlite_connect(db_path)
    now = _utc_now()
    value_json = json.dumps(value, ensure_ascii=False)
    try:
        # Atomic upsert
        conn.execute(
            """INSERT INTO hard_state
               (namespace, key, value_json, version, created_at, updated_at, last_modified_by)
               VALUES (?, ?, ?, 1, ?, ?, ?)
               ON CONFLICT(namespace, key) DO UPDATE SET
               value_json = excluded.value_json,
               version = hard_state.version + 1,
               updated_at = excluded.updated_at,
               last_modified_by = excluded.last_modified_by""",
            (namespace, key, value_json, now, now, modified_by),
        )
        conn.commit()
        
        row = conn.execute(
            "SELECT version FROM hard_state WHERE namespace = ? AND key = ?",
            (namespace, key),
        ).fetchone()
        new_version = row["version"] if row else 1
        
        return {
            "namespace": namespace,
            "key": key,
            "version": new_version,
            "updated_at": now,
        }
    finally:
        conn.close()


def get_state(
    db_path: str | None = None,
    namespace: str = "default",
    key: str = "",
) -> dict | None:
    """Get a hard state value by namespace+key. Returns None if not found."""
    ensure_hard_state_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        row = conn.execute(
            """SELECT namespace, key, value_json, version, created_at, updated_at, last_modified_by
               FROM hard_state
               WHERE namespace = ? AND key = ?""",
            (namespace, key),
        ).fetchone()
        if not row:
            return None
        result = dict(row)
        try:
            result["value"] = json.loads(result.pop("value_json"))
        except (json.JSONDecodeError, TypeError):
            result["value"] = result.pop("value_json")
        return result
    finally:
        conn.close()


def list_state(
    db_path: str | None = None,
    namespace: str = "",
) -> list[dict]:
    """List all keys in a namespace (or all namespaces if empty)."""
    ensure_hard_state_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        if namespace:
            rows = conn.execute(
                """SELECT namespace, key, version, updated_at
                   FROM hard_state WHERE namespace = ?
                   ORDER BY key""",
                (namespace,),
            ).fetchall()
        else:
            rows = conn.execute(
                """SELECT namespace, key, version, updated_at
                   FROM hard_state ORDER BY namespace, key""",
            ).fetchall()
        return [dict(r) for r in rows]
    finally:
        conn.close()


def delete_state(
    db_path: str | None = None,
    namespace: str = "default",
    key: str = "",
) -> bool:
    """Delete a hard state entry. Returns True if deleted."""
    ensure_hard_state_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        conn.execute(
            "DELETE FROM hard_state WHERE namespace = ? AND key = ?",
            (namespace, key),
        )
        return conn.total_changes > 0
    finally:
        conn.close()


# ── derived_items: isolated causal/derived memories ────────────────────────────
# Phase 3: Causal-inferred data lives in a separate table,
# never polluting default search_memory results.

def ensure_derived_items_table(db_path: str | None = None) -> None:
    """Create derived_items table if it doesn't exist."""
    conn = _sqlite_connect(db_path)
    try:
        conn.execute("""
            CREATE TABLE IF NOT EXISTS derived_items (
                id           TEXT PRIMARY KEY,
                path         TEXT NOT NULL DEFAULT '/',
                summary      TEXT NOT NULL DEFAULT '',
                text         TEXT NOT NULL DEFAULT '',
                importance   REAL NOT NULL DEFAULT 0.7,
                timestamp    TEXT NOT NULL,
                source       TEXT NOT NULL DEFAULT 'causal',
                scope        TEXT NOT NULL DEFAULT 'general',
                metadata     TEXT NOT NULL DEFAULT '{}'
            )
        """)
        conn.execute("""
            CREATE INDEX IF NOT EXISTS idx_derived_items_source
            ON derived_items(source)
        """)
        conn.execute("""
            CREATE INDEX IF NOT EXISTS idx_derived_items_timestamp
            ON derived_items(timestamp DESC)
        """)
    finally:
        conn.close()


def save_derived(
    text: str,
    path: str = "/",
    summary: str = "",
    importance: float = 0.7,
    source: str = "causal",
    scope: str = "general",
    metadata: dict | None = None,
    db_path: str | None = None,
) -> str:
    """Save derived insight directly to derived_items without vector embedding."""
    ensure_derived_items_table(db_path)
    entry_id = str(uuid.uuid4())
    now = _utc_now()
    metadata_repr = json.dumps(metadata) if metadata else "{}"
    
    conn = _sqlite_connect(db_path)
    try:
        conn.execute("""
            INSERT INTO derived_items (id, path, summary, text, importance, timestamp, source, scope, metadata)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (entry_id, path, summary, text, importance, now, source, scope, metadata_repr))
        conn.commit()
        return entry_id
    finally:
        conn.close()


def migrate_causal_to_derived(db_path: str | None = None) -> int:
    """Move source='causal' records from memories to derived_items. Returns count moved."""
    ensure_derived_items_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        rows = conn.execute(
            """SELECT id, path, summary, text, importance, timestamp, source, scope, metadata
               FROM memories WHERE source = 'causal'"""
        ).fetchall()

        if not rows:
            return 0

        for row in rows:
            conn.execute(
                """INSERT OR IGNORE INTO derived_items
                   (id, path, summary, text, importance, timestamp, source, scope, metadata)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (row["id"], row["path"], row["summary"], row["text"],
                 row["importance"], row["timestamp"], row["source"],
                 row["scope"], row["metadata"]),
            )

        # Clean up vector index (must do this via rowid before memories table is deleted)
        conn.execute(
            """DELETE FROM memories_vec WHERE rowid IN (
                SELECT m.rowid FROM memories m
                INNER JOIN derived_items d ON m.id = d.id
            )"""
        )
        # Remove migrated records from memories table
        conn.execute("DELETE FROM memories WHERE source = 'causal'")
        # Also clean up FTS index
        conn.execute(
            """DELETE FROM memories_fts WHERE id IN (
                SELECT id FROM derived_items
            )"""
        )
        conn.commit()
        return len(rows)
    finally:
        conn.close()


def list_derived_by_source(
    db_path: str | None = None,
    source: str = "causal",
    path_prefix: str = "",
    limit: int = 100,
) -> list[dict]:
    ensure_derived_items_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        rows = conn.execute(
            """SELECT id, path, summary, text, importance, timestamp, source, scope, metadata
               FROM derived_items
               WHERE source = ? AND path LIKE ?
               ORDER BY timestamp DESC
               LIMIT ?""",
            (source, f"{path_prefix}%", limit),
        ).fetchall()
        return [dict(r) for r in rows]
    finally:
        conn.close()


def count_derived_by_source(
    db_path: str | None = None,
    source: str = "causal",
    path_prefix: str = "",
) -> int:
    ensure_derived_items_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        return conn.execute(
            """SELECT COUNT(*) FROM derived_items
               WHERE source = ? AND path LIKE ?""",
            (source, f"{path_prefix}%"),
        ).fetchone()[0]
    finally:
        conn.close()


def search_derived(
    db_path: str | None = None,
    query: str = "",
    limit: int = 10,
) -> list[dict]:
    """Search derived_items by text (simple LIKE query, no vector search)."""
    ensure_derived_items_table(db_path)
    conn = _sqlite_connect(db_path)
    try:
        if query.strip():
            rows = conn.execute(
                """SELECT id, path, summary, text, importance, timestamp, source
                   FROM derived_items
                   WHERE text LIKE ? OR summary LIKE ?
                   ORDER BY timestamp DESC LIMIT ?""",
                (f"%{query}%", f"%{query}%", limit),
            ).fetchall()
        else:
            rows = conn.execute(
                """SELECT id, path, summary, text, importance, timestamp, source
                   FROM derived_items
                   ORDER BY timestamp DESC LIMIT ?""",
                (limit,),
            ).fetchall()
        return [dict(r) for r in rows]
    finally:
        conn.close()
