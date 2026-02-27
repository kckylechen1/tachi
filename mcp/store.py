"""
SQLite + sqlite-vec storage layer for Antigravity Memory MCP.
Now powered by Rust-based memory-core-py.
"""

import json
import os
import sqlite3
import struct
import uuid
from datetime import datetime, timezone
from typing import Any
from memory_core_py import MemoryStore

DB_PATH = os.environ.get("MEMORY_DB_PATH", os.path.expanduser("~/.sigil/memory.db"))

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
    """Save a memory entry. Returns None if duplicate detected by Rust core."""
    entry_id = str(uuid.uuid4())
    now = datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    
    # Check for near-duplicate before inserting (cosine >= 0.92 → skip)
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
        if results and len(results) > 0:
            if results[0].get("score", {}).get("final", 0) >= 0.92:
                return None
    except Exception:
        # Empty DB or vec0 not ready — skip dedup, proceed to insert
        pass
            
        me = {
            "id": entry_id,
            "path": path,
            "summary": summary,
            "text": text,
            "importance": importance,
            "timestamp": now,
            "category": "fact", # python side mostly deals with facts
            "topic": topic,
            "keywords": keywords or [],
            "persons": [],
            "entities": [],
            "location": "",
            "source": source,
            "scope": scope,
            "vector": vector,
            "metadata": metadata or {}
        }
        store.upsert(json.dumps(me))
        return {
            "id": entry_id, 
            "text": text, 
            "path": path, 
            "summary": summary, 
            "topic": topic, 
            "created_at": now
        }
    except Exception as e:
        print(f"Failed to upsert memory: {e}")
        raise  # Re-raise so server.py can distinguish from dedup skip

def get_memory(store: MemoryStore, entry_id: str, include_archived: bool = False) -> dict | None:
    """Retrieve full memory by ID."""
    res = store.get(entry_id, include_archived)
    if not res:
        return None
    try:
        data = json.loads(res)
        # Adapt field names backwards for old MCP code if necessary
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
                "fts_rank": r["score"]["fts"], # use inner dimension score as fts_rank proxy 
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
        print(f"Hybrid search failed: {e}")
        return []

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
            "memories": memories
        }
    except Exception as e:
        print(f"list_by_path failed: {e}")
        return { "path": normalized_path, "directories": [], "memories": [] }


def get_stats(store: MemoryStore, include_archived: bool = False) -> dict:
    """Get memory db statistics."""
    try:
        res_str = store.get_all(100000, include_archived)
        all_entries = json.loads(res_str)
        count = len(all_entries)
        
        scopes = {}
        root_paths = {}
        for r in all_entries:
            # scope
            s = r.get("scope", "general")
            scopes[s] = scopes.get(s, 0) + 1
            
            # path
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
                # If sqlite-vec isn't available for this sqlite3 connection,
                # keep text/fts updates and let future refresh repair vectors.
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
