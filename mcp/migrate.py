import json
import os
import sqlite3
import hashlib
import time
from memory_core_py import MemoryStore

DB_PATH = os.environ.get("MEMORY_DB_PATH", os.path.expanduser("~/.sigil/memory.db"))

def migrate():
    print(f"Migrating {DB_PATH} ...")
    
    if not os.path.exists(DB_PATH):
        print("No old database found. Done.")
        return
        
    old_db_path = DB_PATH + ".old.sqlite"
    os.rename(DB_PATH, old_db_path)
    print(f"Backed up to {old_db_path}")
    
    # 1. New store setup
    store = MemoryStore(DB_PATH)
    
    # 2. Read from old
    conn = sqlite3.connect(old_db_path)
    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute("SELECT * FROM memories").fetchall()
    except Exception as e:
        print(f"Failed to read memories: {e}")
        return
        
    # sqlite-vec part
    try:
        conn.enable_load_extension(True)
        import sqlite_vec
        sqlite_vec.load(conn)
        vecs = conn.execute("SELECT id, embedding FROM memories_vec").fetchall()
        vec_map = {}
        import struct
        for v in vecs:
            raw = v["embedding"]
            # decode bytes to floats
            num_floats = len(raw) // 4
            floats = struct.unpack(f"{num_floats}f", raw)
            vec_map[v["id"]] = list(floats)
    except sqlite3.OperationalError as e:
        print(f"Operational error reading vectors (maybe no memories_vec table yet?): {e}")
        vec_map = {}
    except Exception as e:
        print(f"Warning: could not read vectors. Maybe no sqlite-vec install? {e}")
        vec_map = {}
        
    # 3. Insert and convert
    count = 0
    for r in rows:
        d = dict(r)
        
        # fix field names
        kw = []
        if d.get("keywords"):
             try:
                 kw = json.loads(d["keywords"])
             except (json.JSONDecodeError, TypeError) as e:
                 print(f"Warning: Failed to parse keywords for entry {d['id']}: {e}")
                 kw = []
                 
        vec = vec_map.get(d["id"])
        
        # fallback default embedding dimension (1024 to match original)
        if not vec:
             vec = [0.0] * 1024 
        
        new_entry = {
            "id": d["id"],
            "path": d.get("path") or "/",
            "summary": d.get("summary") or d["text"][:100],
            "text": d["text"],
            "importance": d.get("importance", 0.7),
            "timestamp": d.get("created_at") or time.strftime("%Y-%m-%dT%H:%M:%S+08:00"),
            "category": "fact",
            "topic": d.get("topic") or "",
            "keywords": kw,
            "persons": [],
            "entities": [],
            "location": "",
            "source": d.get("source") or "manual",
            "scope": d.get("scope") or "general",
            "vector": vec,
            "metadata": {}
        }
        
        try:
            store.upsert(new_entry)
            count += 1
        except Exception as e:
            print(f"Failed to upsert entry {d['id']}: {e}")
            
    print(f"Migrated {count} entries.")

if __name__ == "__main__":
    migrate()
