"""
SQLite + sqlite-vec storage layer for Antigravity Memory MCP.
Now powered by Rust-based memory-core-py.
"""

import json
import time
import os
from memory_core_py import MemoryStore

DB_PATH = os.environ.get("MEMORY_DB_PATH", os.path.expanduser("~/.gemini/antigravity/memory.db"))

def get_connection() -> MemoryStore:
    """Get a database connection powered by the Rust memory-core backend."""
    os.makedirs(os.path.dirname(DB_PATH), exist_ok=True)
    return MemoryStore(DB_PATH)


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
) -> dict | None:
    """Save a memory entry. Returns None if duplicate detected by Rust core."""
    import hashlib
    entry_id = hashlib.sha256(text.encode()).hexdigest()[:16]
    now = time.strftime("%Y-%m-%dT%H:%M:%S+08:00")
    
    # We pass duplicate checking and upsert fully to Rust backend processing
    try:
        # Actually Rust backend doesn't check vector duplication yet 
        # but this mimics what we used to do or what rust backend would do
        opts = json.dumps({"top_k": 1, "query_vec": vector})
        res_str = store.search(text, opts)
        results = json.loads(res_str)
        if results and results[0].get("score", {}).get("final", 0) >= 0.92:
            return None
            
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
            "metadata": {}
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

def get_memory(store: MemoryStore, entry_id: str) -> dict | None:
    """Retrieve full memory by ID."""
    res = store.get(entry_id)
    if not res:
        return None
    try:
        data = json.loads(res)
        # Adapt field names backwards for old MCP code if necessary
        if "timestamp" in data:
            data["created_at"] = data["timestamp"]
        return data
    except:
        return None


def search_by_vector(
    store: MemoryStore,
    query_vec: list[float],
    top_k: int = 8,
    path_prefix: str = "",
) -> list[dict]:
    """Fallback search by vector (pure vector search simulated by 0 lexical weights)."""
    opts = {
        "top_k": top_k,
        "query_vec": query_vec,
        "path_prefix": path_prefix,
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
) -> list[dict]:
    """Fallback search by text (pure FTS search)."""
    opts = {
        "top_k": top_k,
        "path_prefix": path_prefix,
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
    w_vec: float = 0.6,
    w_lex: float = 0.25,
    w_recency: float = 0.15,
) -> list[dict]:
    """Hybrid search delegated to Rust core."""
    opts = {
        "top_k": top_k,
        "query_vec": query_vec,
        "path_prefix": path_prefix,
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

def list_by_path(store: MemoryStore, path_prefix: str) -> dict:
    """List sub-paths and memories immediately under a given path."""
    if not path_prefix.startswith('/'):
        path_prefix = '/' + path_prefix
        
    query_prefix = path_prefix
    if not query_prefix.endswith('/'):
        query_prefix += '/'

    try:
        # Load all from store. Rust layer provides search fallback. Here we just manually scan since limit is 200/500 usually
        res_str = store.get_all(5000) 
        all_entries = json.loads(res_str)
        
        dirs = set()
        memories = []
        
        for r in all_entries:
            p = r.get("path", "")
            if p.startswith(query_prefix) or p == path_prefix:
                if p == path_prefix:
                    # It's in EXACTLY this directory
                    summary = r.get("summary") or (r.get("text", "")[:60] + "...")
                    memories.append({
                        "id": r["id"],
                        "path": p,
                        "summary": summary,
                        "importance": r.get("importance", 0.7),
                        "created_at": r.get("timestamp", ""),
                    })
                else:
                    # It's in a sub-directory
                    rel = p[len(query_prefix):]
                    if '/' in rel:
                        sub_dir = rel.split('/')[0]
                        dirs.add(sub_dir)
                    else:
                        dirs.add(rel)
                        
        return {
            "path": path_prefix,
            "directories": sorted(list(dirs)),
            "memories": memories
        }
    except Exception as e:
        print(f"list_by_path failed: {e}")
        return { "path": path_prefix, "directories": [], "memories": [] }


def get_stats(store: MemoryStore) -> dict:
    """Get memory db statistics."""
    try:
        res_str = store.get_all(100000)
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
