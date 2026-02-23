#!/usr/bin/env python3
"""
Backfill script: reads OpenClaw session transcripts and uses GLM-4v-flash + Voyage-4
to extract and store memories into the bridge's SQLite DB.
"""

import json, os, sys, time, sqlite3, struct, hashlib
import httpx

OPENCLAW_ROOT = os.path.expanduser("~/.openclaw")
DB_PATH = os.path.join(OPENCLAW_ROOT, "workspace/extensions/memory-hybrid-bridge/data/memory.db")

# Load keys from .env
def load_env():
    env = {}
    env_file = os.path.join(OPENCLAW_ROOT, ".env")
    for line in open(env_file):
        line = line.strip()
        if "=" in line and not line.startswith("#"):
            k, v = line.split("=", 1)
            env[k] = v
    return env

ENV = load_env()
GLM_KEY = ENV.get("SILICONFLOW_API_KEY", "") or ENV.get("MEMORY_BRIDGE_OPENAI_API_KEY", "")
GLM_URL = "https://api.siliconflow.cn/v1"
GLM_MODEL = "THUDM/GLM-4-9B-0414"
VOYAGE_KEY = ENV.get("VOYAGE_API_KEY", "")
SINCE = "2026-02-18T08:42:00"

EXTRACT_PROMPT = """你是 memory_builder。从输入窗口提炼值得长期记忆的事实，输出严格 JSON（不要 markdown）。
输出一个 JSON 对象：
{"text": "完整事实复述", "summary": "10-30字摘要", "topic": "主题", "keywords": ["kw1","kw2"], "persons": [], "entities": [], "category": "fact|preference|decision|entity|other", "importance": 0.0-1.0}
约束：不编造信息，无法确定的字段用空字符串或空数组。"""


def find_sessions():
    """Find session files modified since SINCE."""
    sessions = []
    agents_dir = os.path.join(OPENCLAW_ROOT, "agents")
    for agent_id in os.listdir(agents_dir):
        sess_dir = os.path.join(agents_dir, agent_id, "sessions")
        if not os.path.isdir(sess_dir):
            continue
        for f in os.listdir(sess_dir):
            if not f.endswith(".jsonl"):
                continue
            path = os.path.join(sess_dir, f)
            mtime = os.path.getmtime(path)
            if mtime > time.mktime(time.strptime(SINCE[:19], "%Y-%m-%dT%H:%M:%S")):
                sessions.append({"agent_id": agent_id, "session_id": f[:-6], "path": path, "mtime": mtime})
    # Only main/jayne
    return sorted([s for s in sessions if s["agent_id"] in ("main", "jayne")], key=lambda x: x["mtime"])


def read_transcript(path):
    """Parse transcript JSONL into messages."""
    messages = []
    for line in open(path):
        try:
            entry = json.loads(line)
            if entry.get("type") != "message" or not entry.get("message"):
                continue
            msg = entry["message"]
            if msg.get("role") not in ("user", "assistant"):
                continue
            content = msg.get("content", "")
            if isinstance(content, list):
                content = "\n".join(b.get("text", "") for b in content if b.get("type") == "text" and b.get("text"))
            if len(content) > 20 and len(content) < 5000 and not content.startswith("[System"):
                messages.append({"role": msg["role"], "content": content})
        except:
            pass
    return messages


def extract_memory(text):
    """Call GLM-4v-flash to extract a memory entry."""
    r = httpx.post(
        f"{GLM_URL}/chat/completions",
        headers={"Authorization": f"Bearer {GLM_KEY}", "Content-Type": "application/json"},
        json={"model": GLM_MODEL, "temperature": 0, "max_tokens": 500,
              "messages": [{"role": "system", "content": EXTRACT_PROMPT}, {"role": "user", "content": text}]},
        timeout=25,
    )
    r.raise_for_status()
    content = r.json()["choices"][0]["message"]["content"].strip()
    if content.startswith("```"):
        content = content.split("\n", 1)[1].rsplit("```", 1)[0]
    return json.loads(content.strip())


def get_embedding(text):
    """Get Voyage-4 embedding."""
    r = httpx.post(
        "https://api.voyageai.com/v1/embeddings",
        headers={"Authorization": f"Bearer {VOYAGE_KEY}", "Content-Type": "application/json"},
        json={"model": "voyage-4", "input": [text], "input_type": "document"},
        timeout=15,
    )
    r.raise_for_status()
    return r.json()["data"][0]["embedding"]


def serialize_f32(vec):
    return struct.pack(f"{len(vec)}f", *vec)


def save_to_db(conn, entry, vector, agent_id):
    """Save extracted memory to the bridge's SQLite DB."""
    entry_id = "bf_" + hashlib.sha256(entry["text"].encode()).hexdigest()[:12]
    now = time.strftime("%Y-%m-%dT%H:%M:%S.000Z")
    metadata = json.dumps({
        "keywords": entry.get("keywords", []),
        "persons": entry.get("persons", []),
        "entities": entry.get("entities", []),
        "topic": entry.get("topic", ""),
        "scope": "project",
        "category": entry.get("category", "fact"),
        "location": "",
        "source_refs": [{"ref_type": "message", "ref_id": f"backfill_{agent_id}"}],
    }, ensure_ascii=False)

    # Check if exists
    existing = conn.execute("SELECT id FROM memories WHERE id = ?", (entry_id,)).fetchone()
    if existing:
        return None

    conn.execute(
        "INSERT INTO memories (id, path, summary, text, importance, created_at, metadata, vector) VALUES (?,?,?,?,?,?,?,?)",
        (entry_id, f"/openclaw/agent-{agent_id}", entry.get("summary", entry["text"][:80]),
         entry["text"], entry.get("importance", 0.7), now, metadata, json.dumps(vector) if vector else None)
    )

    # Also insert into vec table if it exists
    try:
        if vector and len(vector) == 1024:
            conn.execute("INSERT OR REPLACE INTO memories_vec (id, embedding) VALUES (?, ?)",
                         (entry_id, serialize_f32(vector)))
    except:
        pass  # vec table might not exist yet

    conn.commit()
    return entry_id


def main():
    import traceback
    sessions = find_sessions()
    print(f"Found {len(sessions)} sessions since {SINCE}", flush=True)
    print(f"GLM key: {GLM_KEY[:10]}..., Voyage key: {VOYAGE_KEY[:10]}...", flush=True)

    conn = sqlite3.connect(DB_PATH)
    conn.execute("PRAGMA journal_mode=WAL")
    total_saved, total_skipped = 0, 0

    for sess in sessions:
        print(f"\n--- {sess['agent_id']}/{sess['session_id'][:8]}... ---", flush=True)
        try:
            msgs = read_transcript(sess["path"])
            print(f"  Messages: {len(msgs)}", flush=True)
            if len(msgs) < 2:
                print("  Skip: too few messages", flush=True)
                continue

            # Process in chunks of 6
            for i in range(0, len(msgs), 6):
                chunk = msgs[i:i + 6]
                window = "\n\n".join(f"[{j}] {m['role']}: {m['content']}" for j, m in enumerate(chunk))
                if len(window) < 50:
                    continue

                try:
                    entry = extract_memory(window)
                    if not entry or not entry.get("text"):
                        print(f"  Chunk {i}: no extraction", flush=True)
                        continue

                    vec = get_embedding(entry["text"])
                    eid = save_to_db(conn, entry, vec, sess["agent_id"])
                    if eid:
                        print(f"  Chunk {i}: ✅ {entry['text'][:80]}...", flush=True)
                        total_saved += 1
                    else:
                        print(f"  Chunk {i}: ⏭ duplicate", flush=True)
                        total_skipped += 1

                    time.sleep(0.3)  # Rate limit
                except Exception as e:
                    print(f"  Chunk {i}: error: {e}", flush=True)
                    traceback.print_exc()
        except Exception as e:
            print(f"  Session error: {e}", flush=True)
            traceback.print_exc()

    conn.close()
    print(f"\n=== Done: {total_saved} saved, {total_skipped} skipped ===", flush=True)


if __name__ == "__main__":
    main()

