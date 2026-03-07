// db.rs — SQLite storage engine using rusqlite
//
// Replaces JS `store.ts` (MemoryStore class, getDB, searchVec, searchFTS)
// and Python `store.py` (get_connection, save_memory, search_by_vector, search_by_text).
//
// All hot-path row iteration and data conversion happens here in Rust —
// zero JSON round-trips to JS/Python during search.

use rusqlite::{params, Connection, Result as SqlResult};
use std::collections::HashMap;
use chrono::{DateTime, SecondsFormat, Utc};
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
            archived     INTEGER NOT NULL DEFAULT 0,
            created_at   TEXT NOT NULL DEFAULT '',
            updated_at   TEXT NOT NULL DEFAULT '',
            access_count INTEGER NOT NULL DEFAULT 0,
            last_access  TEXT,
            metadata     TEXT NOT NULL DEFAULT '{}'
        );

        CREATE INDEX IF NOT EXISTS idx_memories_path        ON memories(path);
        CREATE INDEX IF NOT EXISTS idx_memories_importance  ON memories(importance DESC);
        CREATE INDEX IF NOT EXISTS idx_memories_timestamp   ON memories(timestamp DESC);

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

    // Forward-compatible migrations for existing DB files created before
    // archived/created_at/updated_at columns existed.
    ensure_column(
        conn,
        "memories",
        "archived",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "memories",
        "created_at",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "memories",
        "updated_at",
        "TEXT NOT NULL DEFAULT ''",
    )?;

    // Indexes on migrated columns — MUST come after ensure_column so the
    // columns exist on legacy databases that were created without them.
    conn.execute_batch(r#"
        CREATE INDEX IF NOT EXISTS idx_memories_archived    ON memories(archived);
        CREATE INDEX IF NOT EXISTS idx_memories_last_access ON memories(last_access DESC);
    "#)?;

    // Backfill empty values for legacy rows.
    conn.execute(
        "UPDATE memories SET created_at = timestamp WHERE created_at IS NULL OR created_at = ''",
        [],
    )?;
    conn.execute(
        "UPDATE memories SET updated_at = created_at WHERE updated_at IS NULL OR updated_at = ''",
        [],
    )?;

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

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), MemoryError> {
    if has_column(conn, table, column)? {
        return Ok(());
    }

    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
    conn.execute(&sql, [])?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, MemoryError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ─── Row mapping ──────────────────────────────────────────────────────────────

#[inline]
fn now_utc_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[inline]
fn normalize_utc_iso(ts: &str) -> Result<String, MemoryError> {
    let raw = ts.trim();
    if raw.is_empty() {
        return Err(MemoryError::InvalidArg("empty timestamp".to_string()));
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true));
    }

    if let Ok(dt) = raw.parse::<DateTime<Utc>>() {
        return Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true));
    }

    Err(MemoryError::InvalidArg(format!("invalid timestamp format: {}", ts)))
}

#[inline]
pub fn normalize_utc_iso_or_now(ts: &str) -> String {
    normalize_utc_iso(ts).unwrap_or_else(|_| now_utc_iso())
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
        archived: row.get("archived")?,
        access_count: row.get("access_count")?,
        last_access,
        metadata,
        vector: None,
    })
}

// ─── UPSERT ───────────────────────────────────────────────────────────────────

/// Insert or update a memory entry (and its embedding vector if provided).
pub fn upsert(conn: &Connection, entry: &MemoryEntry, vec_available: bool) -> Result<(), MemoryError> {
    if entry.id.trim().is_empty() {
        return Err(MemoryError::InvalidArg("entry.id must be provided by caller".to_string()));
    }

    let timestamp_utc = normalize_utc_iso(&entry.timestamp)?;
    let last_access_utc = entry.last_access.as_deref().map(normalize_utc_iso).transpose()?;
    let write_time_utc = now_utc_iso();

    let metadata_json = serde_json::to_string(&entry.metadata)?;
    let kws_json = serde_json::to_string(&entry.keywords)?;
    let p_json = serde_json::to_string(&entry.persons)?;
    let e_json = serde_json::to_string(&entry.entities)?;

    // All writes for one upsert must be atomic across main table + FTS + vec.
    let tx = conn.unchecked_transaction()?;

    // Write to main table
    tx.execute(
        r#"INSERT INTO memories
              (id, path, summary, text, importance,
               timestamp, category, topic, keywords, persons, entities,
               location, source, scope, archived, created_at, updated_at,
               access_count, last_access, metadata)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
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
               archived     = excluded.archived,
               created_at   = memories.created_at,
               updated_at   = excluded.updated_at,
               access_count = excluded.access_count,
               last_access  = excluded.last_access,
               metadata     = excluded.metadata"#,
        params![
            entry.id,
            entry.path,
            entry.summary,
            entry.text,
            entry.importance,
            timestamp_utc,
            entry.category,
            entry.topic,
            kws_json,
            p_json,
            e_json,
            entry.location,
            entry.source,
            entry.scope,
            entry.archived,
            &write_time_utc,
            &write_time_utc,
            entry.access_count,
            last_access_utc,
            metadata_json,
        ],
    )?;

    // Sync FTS: delete old row (if any) then re-insert
    let kws = entry.keywords.join(" ");
    let ents = entry.entities.join(" ");
    tx.execute(
        "DELETE FROM memories_fts WHERE id = ?1",
        params![entry.id],
    )?;
    tx.execute(
        "INSERT INTO memories_fts(id, path, summary, text, keywords, entities)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![entry.id, entry.path, entry.summary, entry.text, kws, ents],
    )?;

    if let Some(vec) = &entry.vector {
        if vec_available {
            let blob = serialize_f32(vec);
            // vec0 virtual tables do NOT support ON CONFLICT / UPSERT.
            // Use DELETE + INSERT (same pattern as FTS sync above).
            tx.execute(
                "DELETE FROM memories_vec WHERE id = ?1",
                params![entry.id],
            )?;
            tx.execute(
                "INSERT INTO memories_vec(id, embedding) VALUES (?1, ?2)",
                params![entry.id, blob],
            )?;
        }
    }

    tx.commit()?;
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
    include_archived: bool,
) -> Result<HashMap<String, f64>, MemoryError> {
    let blob = serialize_f32(query_vec);
    let mut stmt = conn.prepare(
        r#"SELECT v.id, v.distance
           FROM memories_vec v
           JOIN memories m ON m.id = v.id
           WHERE v.embedding MATCH ?1
             AND k = ?3
             AND (?2 = 1 OR m.archived = 0)
           ORDER BY v.distance"#,
    )?;

    let rows = stmt.query_map(params![blob, include_archived as i64, top_k as i64], |row| {
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
    include_archived: bool,
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
        r#"SELECT memories_fts.id, -bm25(memories_fts) AS score
           FROM memories_fts
           JOIN memories m ON m.id = memories_fts.id
           WHERE memories_fts MATCH simple_query(?1)
             AND (?2 = 1 OR m.archived = 0)
           ORDER BY bm25(memories_fts)
           LIMIT ?3"#,
    )?;

    let rows = stmt.query_map(params![safe_query, include_archived as i64, limit as i64], |row| {
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
/// Also hydrates vectors from memories_vec if available.
pub fn fetch_by_ids(
    conn: &Connection,
    ids: &[String],
    include_archived: bool,
) -> Result<HashMap<String, MemoryEntry>, MemoryError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Build a parameterised IN clause
    let placeholders = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect::<Vec<_>>().join(",");
    let mut sql = format!(
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,archived,access_count,last_access,metadata
         FROM memories WHERE id IN ({})",
        placeholders
    );
    if !include_archived {
        sql.push_str(" AND archived = 0");
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_entry)?;

    let mut out = HashMap::new();
    for r in rows {
        let entry = r?;
        out.insert(entry.id.clone(), entry);
    }

    // Hydrate vectors from memories_vec (best-effort: table may not exist)
    let vec_sql = format!(
        "SELECT id, embedding FROM memories_vec WHERE id IN ({})",
        placeholders
    );
    if let Ok(mut vec_stmt) = conn.prepare(&vec_sql) {
        if let Ok(vec_rows) = vec_stmt.query_map(
            rusqlite::params_from_iter(ids.iter()),
            |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, blob))
            },
        ) {
            for r in vec_rows.flatten() {
                let (id, blob) = r;
                if let Some(entry) = out.get_mut(&id) {
                    // Deserialize f32 vector from blob
                    if blob.len() % 4 == 0 {
                        let vec: Vec<f32> = blob
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect();
                        entry.vector = Some(vec);
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Fetch the most recent entries, up to `limit`. Returns sorted dynamically by inserted time.
pub fn get_all(
    conn: &Connection,
    limit: usize,
    include_archived: bool,
) -> Result<Vec<MemoryEntry>, MemoryError> {
    let sql = if include_archived {
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,archived,access_count,last_access,metadata
         FROM memories ORDER BY timestamp DESC LIMIT ?"
    } else {
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,archived,access_count,last_access,metadata
         FROM memories WHERE archived = 0 ORDER BY timestamp DESC LIMIT ?"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![limit], row_to_entry)?;
    
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Fetch entries under a path prefix using SQL pushdown instead of full-table scans.
pub fn list_by_path(
    conn: &Connection,
    path_prefix: &str,
    limit: usize,
    include_archived: bool,
) -> Result<Vec<MemoryEntry>, MemoryError> {
    let mut normalized = path_prefix.trim().to_string();
    if normalized.is_empty() {
        normalized = "/".to_string();
    }
    if !normalized.starts_with('/') {
        normalized = format!("/{normalized}");
    }
    if normalized.len() > 1 {
        normalized = normalized.trim_end_matches('/').to_string();
    }
    let like_prefix = if normalized == "/" {
        "/%".to_string()
    } else {
        format!("{normalized}/%")
    };

    let sql = if include_archived {
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,archived,access_count,last_access,metadata
         FROM memories
         WHERE path = ?1 OR path LIKE ?2
         ORDER BY path ASC, timestamp DESC
         LIMIT ?3"
    } else {
        "SELECT id,path,summary,text,importance,timestamp,category,topic,keywords,persons,entities,location,source,scope,archived,access_count,last_access,metadata
         FROM memories
         WHERE (path = ?1 OR path LIKE ?2) AND archived = 0
         ORDER BY path ASC, timestamp DESC
         LIMIT ?3"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![normalized, like_prefix, limit], row_to_entry)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Bump access_count and last_access for a list of IDs (called after every search).
pub fn record_access(conn: &Connection, ids: &[String]) -> Result<(), MemoryError> {
    if ids.is_empty() {
        return Ok(());
    }

    let now = now_utc_iso();
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        tx.execute(
            "UPDATE memories
             SET access_count = access_count + 1, last_access = ?1, updated_at = ?1
             WHERE id = ?2",
            params![&now, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// ─── DELETE ───────────────────────────────────────────────────────────────────

/// Delete a memory entry by ID from main table, FTS index, and vector table.
/// Returns true if an entry was found and deleted.
pub fn delete(conn: &Connection, id: &str, vec_available: bool) -> Result<bool, MemoryError> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(MemoryError::InvalidArg("empty ID".to_string()));
    }

    let tx = conn.unchecked_transaction()?;

    // Delete from main table and check if anything was actually removed
    tx.execute("DELETE FROM memories WHERE id = ?1", params![trimmed])?;
    let deleted = tx.changes() > 0;

    if deleted {
        // Clean up FTS index
        tx.execute("DELETE FROM memories_fts WHERE id = ?1", params![trimmed])?;

        // Clean up vector table (best-effort)
        if vec_available {
            let _ = tx.execute("DELETE FROM memories_vec WHERE id = ?1", params![trimmed]);
        }
    }

    tx.commit()?;
    Ok(deleted)
}

// ─── STATS ────────────────────────────────────────────────────────────────────

/// Get aggregate statistics about the memory store.
pub fn stats(conn: &Connection, include_archived: bool) -> Result<crate::types::StatsResult, MemoryError> {
    fn i64_to_u64(value: i64, label: &str) -> Result<u64, MemoryError> {
        u64::try_from(value)
            .map_err(|_| MemoryError::InvalidArg(format!("negative aggregate count for {label}")))
    }

    fn aggregate_counts(
        conn: &Connection,
        sql: &str,
        include_archived: bool,
        label: &str,
    ) -> Result<HashMap<String, u64>, MemoryError> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![include_archived as i64], |row| {
            let key: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((key, count))
        })?;

        let mut out = HashMap::new();
        for row in rows {
            let (key, count) = row?;
            out.insert(key, i64_to_u64(count, label)?);
        }
        Ok(out)
    }

    fn root_path(path: &str) -> String {
        let mut parts = path.split('/').filter(|part| !part.is_empty());
        match parts.next() {
            Some(root) => format!("/{root}"),
            None => "/".to_string(),
        }
    }

    let total_i64: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE (?1 = 1 OR archived = 0)",
        params![include_archived as i64],
        |row| row.get(0),
    )?;
    let total = i64_to_u64(total_i64, "total")?;

    let by_scope = aggregate_counts(
        conn,
        "SELECT scope, COUNT(*) FROM memories
         WHERE (?1 = 1 OR archived = 0)
         GROUP BY scope",
        include_archived,
        "scope",
    )?;
    let by_category = aggregate_counts(
        conn,
        "SELECT category, COUNT(*) FROM memories
         WHERE (?1 = 1 OR archived = 0)
         GROUP BY category",
        include_archived,
        "category",
    )?;

    let mut stmt = conn.prepare(
        "SELECT path FROM memories WHERE (?1 = 1 OR archived = 0)"
    )?;
    let rows = stmt.query_map(params![include_archived as i64], |row| row.get::<_, String>(0))?;
    let mut by_root_path: HashMap<String, u64> = HashMap::new();
    for row in rows {
        let path = row?;
        *by_root_path.entry(root_path(&path)).or_insert(0) += 1;
    }

    Ok(crate::types::StatsResult {
        total,
        by_scope,
        by_category,
        by_root_path,
    })
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
            archived: false,
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

        let results = search_fts(&conn, "systems programming", 5, false).unwrap();
        assert!(results.contains_key("abc"), "expected 'abc' in FTS results");
    }

    #[test]
    fn upsert_idempotent() {
        let conn = make_conn();
        let mut e = make_entry("dup", "first text");
        upsert(&conn, &e, false).unwrap();
        e.text = "updated text".into();
        upsert(&conn, &e, false).unwrap();

        let results = search_fts(&conn, "updated", 5, false).unwrap();
        assert!(results.contains_key("dup"));
    }

    #[test]
    fn search_vec_knn_with_k_constraint() {
        let conn = make_conn();
        let has_vec: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'memories_vec'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if has_vec == 0 {
            return;
        }

        let mut e = make_entry("vec-1", "vector memory entry");
        e.vector = Some(vec![0.1_f32; 1024]);
        upsert(&conn, &e, true).unwrap();

        let query = vec![0.1_f32; 1024];
        let results = search_vec(&conn, &query, 3, false).unwrap();
        assert!(results.contains_key("vec-1"));
    }

    #[test]
    fn delete_existing() {
        let conn = make_conn();
        let e = make_entry("del-1", "to be deleted");
        upsert(&conn, &e, false).unwrap();

        let deleted = delete(&conn, "del-1", false).unwrap();
        assert!(deleted, "should return true for existing entry");

        // Verify it's gone from main table
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories WHERE id = 'del-1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify it's gone from FTS
        let fts_results = search_fts(&conn, "deleted", 5, false).unwrap();
        assert!(!fts_results.contains_key("del-1"));
    }

    #[test]
    fn delete_nonexistent() {
        let conn = make_conn();
        let deleted = delete(&conn, "nonexistent-id", false).unwrap();
        assert!(!deleted, "should return false for non-existent entry");
    }

    #[test]
    fn stats_aggregation() {
        let conn = make_conn();

        let mut e1 = make_entry("s1", "fact entry");
        e1.scope = "general".into();
        e1.category = "fact".into();
        e1.path = "/project/alpha".into();
        upsert(&conn, &e1, false).unwrap();

        let mut e2 = make_entry("s2", "decision entry");
        e2.scope = "project".into();
        e2.category = "decision".into();
        e2.path = "/project/beta".into();
        upsert(&conn, &e2, false).unwrap();

        let mut e3 = make_entry("s3", "user preference");
        e3.scope = "user".into();
        e3.category = "preference".into();
        e3.path = "/user/settings".into();
        upsert(&conn, &e3, false).unwrap();

        let s = stats(&conn, false).unwrap();
        assert_eq!(s.total, 3);
        assert_eq!(s.by_scope.get("general"), Some(&1_u64));
        assert_eq!(s.by_scope.get("project"), Some(&1_u64));
        assert_eq!(s.by_scope.get("user"), Some(&1_u64));
        assert_eq!(s.by_category.get("fact"), Some(&1_u64));
        assert_eq!(s.by_category.get("decision"), Some(&1_u64));
        assert_eq!(s.by_root_path.get("/project"), Some(&2_u64));
        assert_eq!(s.by_root_path.get("/user"), Some(&1_u64));
    }
}
