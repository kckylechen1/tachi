// db.rs — SQLite storage engine using rusqlite
//
// Replaces JS `store.ts` (MemoryStore class, getDB, searchVec, searchFTS)
// and Python `store.py` (get_connection, save_memory, search_by_vector, search_by_text).
//
// All hot-path row iteration and data conversion happens here in Rust —
// zero JSON round-trips to JS/Python during search.

use rusqlite::{params, Connection, Result as SqlResult};
use std::collections::HashMap;
use chrono::{DateTime, Utc};
use serde_json;

use crate::error::MemoryError;
use crate::types::MemoryEntry;

/// Initialise all required tables on a fresh or existing database.
/// Idempotent: safe to call on every startup.
/// NOTE: `libsimple::enable_auto_extension()` must be called BEFORE opening
/// the Connection. See `MemoryStore::open()` in lib.rs.
pub fn init_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA cache_size = -16000;   -- 16 MB page cache

        CREATE TABLE IF NOT EXISTS memories (
            id           TEXT PRIMARY KEY,
            path         TEXT NOT NULL DEFAULT '/',
            summary      TEXT NOT NULL DEFAULT '',
            text         TEXT NOT NULL DEFAULT '',
            importance   REAL NOT NULL DEFAULT 0.7,
            timestamp    TEXT NOT NULL,
            category     TEXT NOT NULL DEFAULT 'fact',
            topic        TEXT NOT NULL DEFAULT '',
            keywords     TEXT NOT NULL DEFAULT '[]',
            persons      TEXT NOT NULL DEFAULT '[]',
            entities     TEXT NOT NULL DEFAULT '[]',
            location     TEXT NOT NULL DEFAULT '',
            source       TEXT NOT NULL DEFAULT 'manual',
            scope        TEXT NOT NULL DEFAULT 'general',
            access_count INTEGER NOT NULL DEFAULT 0,
            last_access  TEXT,
            metadata     TEXT NOT NULL DEFAULT '{}'
        );

        CREATE INDEX IF NOT EXISTS idx_memories_path        ON memories(path);
        CREATE INDEX IF NOT EXISTS idx_memories_importance  ON memories(importance DESC);
        CREATE INDEX IF NOT EXISTS idx_memories_timestamp   ON memories(timestamp DESC);
        CREATE INDEX IF NOT EXISTS idx_memories_last_access ON memories(last_access DESC);

        -- Standalone FTS5 table with Chinese + Pinyin tokenizer.
        -- Uses wangfenjin/simple for CJK segmentation.
        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            id UNINDEXED,
            path,
            summary,
            text,
            keywords,
            entities,
            tokenize = 'simple'
        );
    "#)?;

    // NOTE: sqlite-vec virtual table (memories_vec) is created separately after
    // the extension is loaded by the caller via register_sqlite_vec().
    Ok(())
}

/// Register the sqlite-vec extension globally via sqlite3_auto_extension.
/// Must be called ONCE before any Connection::open(). Safe to call multiple times.
pub fn register_sqlite_vec() {
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())
        ));
    }
}

/// Attempt to create the sqlite-vec virtual table so KNN search is available.
/// Assumes `register_sqlite_vec()` was called before the connection was opened.
/// Returns true if the vec0 table was created successfully, false otherwise.
pub fn try_load_sqlite_vec(conn: &Connection) -> bool {
    let r: SqlResult<()> = conn.execute_batch(r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
            id      TEXT PRIMARY KEY,
            embedding float[1024]
        );
    "#);
    r.is_ok()
}

/// Serialise a float32 vector into the binary blob format sqlite-vec expects.
pub fn serialize_f32(vec: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(vec.len() * 4);
    for &v in vec {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

// ─── Row mapping ──────────────────────────────────────────────────────────────

fn parse_dt(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> SqlResult<MemoryEntry> {
    let metadata_str: String = row.get("metadata")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({}));

    let last_access = row.get("last_access").unwrap_or(None);

    Ok(MemoryEntry {
        id: row.get("id")?,
        path: row.get("path")?,
        summary: row.get("summary")?,
        text: row.get("text")?,
        importance: row.get("importance")?,
        timestamp: row.get("timestamp")?,
        category: row.get("category")?,
        topic: row.get("topic")?,
        keywords: serde_json::from_str(&row.get::<_, String>("keywords")?).unwrap_or_default(),
        persons: serde_json::from_str(&row.get::<_, String>("persons")?).unwrap_or_default(),
        entities: serde_json::from_str(&row.get::<_, String>("entities")?).unwrap_or_default(),
        location: row.get("location")?,
        source: row.get("source")?,
        scope: row.get("scope")?,
        access_count: row.get("access_count")?,
        last_access,
        metadata,
        vector: None,
    })
}

// ─── UPSERT ───────────────────────────────────────────────────────────────────

/// Insert or update a memory entry (and its embedding vector if provided).
pub fn upsert(conn: &Connection, entry: &MemoryEntry, vec_available: bool) -> Result<(), MemoryError> {
    let metadata_json = serde_json::to_string(&entry.metadata)?;
    let kws_json = serde_json::to_string(&entry.keywords)?;
    let p_json = serde_json::to_string(&entry.persons)?;
    let e_json = serde_json::to_string(&entry.entities)?;

    // Write to main table
    conn.execute(
        r#"INSERT INTO memories
              (id, path, summary, text, importance,
               timestamp, category, topic, keywords, persons, entities,
               location, source, scope, access_count, last_access, metadata)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
           ON CONFLICT(id) DO UPDATE SET
               path         = excluded.path,
               summary      = excluded.summary,
               text         = excluded.text,
               importance   = excluded.importance,
               timestamp    = excluded.timestamp,
               category     = excluded.category,
               topic        = excluded.topic,
               keywords     = excluded.keywords,
               persons      = excluded.persons,
               entities     = excluded.entities,
               location     = excluded.location,
               source       = excluded.source,
               scope        = excluded.scope,
               access_count = excluded.access_count,
               last_access  = excluded.last_access,
               metadata     = excluded.metadata"#,
        params![
            entry.id,
            entry.path,
            entry.summary,
            entry.text,
            entry.importance,
            entry.timestamp,
            entry.category,
            entry.topic,
            kws_json,
            p_json,
            e_json,
            entry.location,
            entry.source,
            entry.scope,
            entry.access_count,
            entry.last_access,
            metadata_json,
        ],
    )?;

    // Sync FTS: delete old row (if any) then re-insert
    let kws = entry.keywords.join(" ");
    let ents = entry.entities.join(" ");
    conn.execute(
        "DELETE FROM memories_fts WHERE id = ?1",
        params![entry.id],
    )?;
    conn.execute(
        "INSERT INTO memories_fts(id, path, summary, text, keywords, entities)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![entry.id, entry.path, entry.summary, entry.text, kws, ents],
    )?;

    if let Some(vec) = &entry.vector {
        if vec_available {
            let blob = serialize_f32(vec);
            conn.execute(
                "INSERT INTO memories_vec(id, embedding) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET embedding = excluded.embedding",
                params![entry.id, blob],
            )?;
        }
    }

    Ok(())
}

// ─── VECTOR SEARCH ────────────────────────────────────────────────────────────

/// KNN vector search via sqlite-vec.
/// Returns (doc_id → cosine_distance) for the top `top_k` results.
/// Cosine *distance* is in [0, 2]; we convert to similarity [0, 1]:
///   similarity = 1 − distance/2
pub fn search_vec(
    conn: &Connection,
    query_vec: &[f32],
    top_k: usize,
) -> Result<HashMap<String, f64>, MemoryError> {
    let blob = serialize_f32(query_vec);
    let mut stmt = conn.prepare(
        r#"SELECT v.id, v.distance
           FROM memories_vec v
           WHERE v.embedding MATCH ?1
           ORDER BY v.distance
           LIMIT ?2"#,
    )?;

    let rows = stmt.query_map(params![blob, top_k as i64], |row| {
        let id: String = row.get(0)?;
        let dist: f64 = row.get(1)?;
        Ok((id, dist))
    })?;

    let mut scores = HashMap::new();
    for r in rows {
        let (id, dist) = r?;
        // sqlite-vec returns L2 / cosine distance depending on vec0 config;
        // treat as cosine distance ∈ [0, 2] → similarity ∈ [0, 1]
        let sim = (1.0 - dist / 2.0).clamp(0.0, 1.0);
        scores.insert(id, sim);
    }
    Ok(scores)
}

// ─── FTS SEARCH ───────────────────────────────────────────────────────────────

/// Full-text search using the FTS5 virtual table.
/// Returns (doc_id → normalised BM25 score [0, 1]).
pub fn search_fts(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<HashMap<String, f64>, MemoryError> {
    // Sanitise query: remove potentially dangerous characters
    let safe_query: String = query
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || matches!(c, '"' | '\'' | '-' | '_'))
        .collect();

    if safe_query.trim().is_empty() {
        return Ok(HashMap::new());
    }

    // Use simple_query() for automatic CJK segmentation in MATCH clause
    let mut stmt = conn.prepare(
        r#"SELECT id, -bm25(memories_fts) AS score
           FROM memories_fts
           WHERE memories_fts MATCH simple_query(?1)
           ORDER BY bm25(memories_fts)
           LIMIT ?2"#,
    )?;

    let rows = stmt.query_map(params![safe_query, limit as i64], |row| {
        let id: String = row.get(0)?;
        let score: f64 = row.get(1)?;
        Ok((id, score))
    })?;

    let mut raw: Vec<(String, f64)> = rows.filter_map(|r| r.ok()).collect();
    if raw.is_empty() {
        return Ok(HashMap::new());
    }

    // Normalise BM25 scores to [0, 1] based on the max in this result set
    let max_score = raw.iter().map(|(_, s)| *s).fold(f64::NEG_INFINITY, f64::max);
    let max_score = if max_score <= 0.0 { 1.0 } else { max_score };

    Ok(raw
        .drain(..)
        .map(|(id, s)| (id, (s / max_score).clamp(0.0, 1.0)))
        .collect())
}

// ─── BULK FETCH ───────────────────────────────────────────────────────────────

/// Fetch multiple entries by their IDs in one query.
pub fn fetch_by_ids(
    conn: &Connection,
    ids: &[String],
) -> Result<HashMap<String, MemoryEntry>, MemoryError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Build a parameterised IN clause
    let placeholders = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,access_count,last_access,metadata
         FROM memories WHERE id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_entry)?;

    let mut out = HashMap::new();
    for r in rows {
        let entry = r?;
        out.insert(entry.id.clone(), entry);
    }
    Ok(out)
}

/// Fetch the most recent entries, up to `limit`. Returns sorted dynamically by inserted time.
pub fn get_all(conn: &Connection, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
    let sql = "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,access_count,last_access,metadata
               FROM memories ORDER BY timestamp DESC LIMIT ?";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![limit], row_to_entry)?;
    
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Bump access_count and last_access for a list of IDs (called after every search).
pub fn record_access(conn: &Connection, ids: &[String]) -> Result<(), MemoryError> {
    let now = Utc::now().to_rfc3339();
    for id in ids {
        conn.execute(
            "UPDATE memories SET access_count = access_count + 1, last_access = ?1 WHERE id = ?2",
            params![now, id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn make_conn() -> Connection {
        libsimple::enable_auto_extension().unwrap();
        register_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        try_load_sqlite_vec(&conn);
        conn
    }

    fn make_entry(id: &str, text: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            path: "/test".into(),
            summary: text[..text.len().min(30)].into(),
            text: text.into(),
            importance: 0.7,
            timestamp: Utc::now().to_rfc3339(),
            category: "fact".into(),
            topic: "".into(),
            keywords: vec!["test".into()],
            persons: vec![],
            entities: vec![],
            location: "".into(),
            source: "".into(),
            scope: "general".into(),
            access_count: 0,
            last_access: None,
            metadata: json!({ "keywords": ["test"], "entities": [] }),
            vector: None,
        }
    }

    #[test]
    fn upsert_and_fts() {
        let conn = make_conn();
        let e = make_entry("abc", "Rust is a systems programming language");
        upsert(&conn, &e, false).unwrap();

        let results = search_fts(&conn, "systems programming", 5).unwrap();
        assert!(results.contains_key("abc"), "expected 'abc' in FTS results");
    }

    #[test]
    fn upsert_idempotent() {
        let conn = make_conn();
        let mut e = make_entry("dup", "first text");
        upsert(&conn, &e, false).unwrap();
        e.text = "updated text".into();
        upsert(&conn, &e, false).unwrap();

        let results = search_fts(&conn, "updated", 5).unwrap();
        assert!(results.contains_key("dup"));
    }
}
