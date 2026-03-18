// db.rs — SQLite storage engine using rusqlite
//
// Replaces JS `store.ts` (MemoryStore class, getDB, searchVec, searchFTS)
// and Python `store.py` (get_connection, save_memory, search_by_vector, search_by_text).
//
// All hot-path row iteration and data conversion happens here in Rust —
// zero JSON round-trips to JS/Python during search.

use rusqlite::{params, Connection, Result as SqlResult};
use std::collections::HashMap;
use std::sync::Once;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json;
use uuid::Uuid;

use crate::error::MemoryError;
use crate::types::MemoryEntry;

static SQLITE_VEC_AUTO_EXT_ONCE: Once = Once::new();

/// Initialise all required tables on a fresh or existing database.
/// Idempotent: safe to call on every startup.
/// NOTE: `libsimple::enable_auto_extension()` must be called BEFORE opening
/// the Connection. See `MemoryStore::open()` in lib.rs.
pub fn init_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
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

        -- Memory graph edges for causal/temporal/entity relationships
        CREATE TABLE IF NOT EXISTS memory_edges (
            source_id  TEXT NOT NULL,
            target_id  TEXT NOT NULL,
            relation   TEXT NOT NULL,
            weight     REAL NOT NULL DEFAULT 1.0,
            metadata   TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (source_id, target_id, relation)
        );
        CREATE INDEX IF NOT EXISTS idx_edges_source ON memory_edges(source_id);
        CREATE INDEX IF NOT EXISTS idx_edges_target ON memory_edges(target_id);
        CREATE INDEX IF NOT EXISTS idx_edges_relation ON memory_edges(relation);

        -- Deterministic KV state (no vector search, no LLM)
        CREATE TABLE IF NOT EXISTS hard_state (
            namespace        TEXT NOT NULL,
            key              TEXT NOT NULL,
            value_json       TEXT NOT NULL DEFAULT '{}',
            version          INTEGER NOT NULL DEFAULT 1,
            created_at       TEXT NOT NULL DEFAULT '',
            updated_at       TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (namespace, key)
        );

        -- Access history for ACT-R base-level activation
        CREATE TABLE IF NOT EXISTS access_history (
            memory_id  TEXT NOT NULL,
            accessed_at TEXT NOT NULL,
            query_hash  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_access_hist_mem ON access_history(memory_id);
        CREATE INDEX IF NOT EXISTS idx_access_hist_time ON access_history(accessed_at DESC);

        -- Derived items (causal extractions, distilled rules, etc.)
        CREATE TABLE IF NOT EXISTS derived_items (
            id         TEXT PRIMARY KEY,
            text       TEXT NOT NULL DEFAULT '',
            path       TEXT NOT NULL DEFAULT '/',
            summary    TEXT NOT NULL DEFAULT '',
            importance REAL NOT NULL DEFAULT 0.5,
            source     TEXT NOT NULL DEFAULT '',
            scope      TEXT NOT NULL DEFAULT 'general',
            metadata   TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT ''
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

    // Temporal edge columns for memory_edges
    ensure_column(conn, "memory_edges", "valid_from", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "memory_edges", "valid_to", "TEXT")?;

    // derived_items columns that may be missing on legacy databases
    ensure_column(conn, "derived_items", "summary", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "derived_items", "importance", "REAL NOT NULL DEFAULT 0.5")?;
    ensure_column(conn, "derived_items", "scope", "TEXT NOT NULL DEFAULT 'general'")?;
    ensure_column(conn, "derived_items", "created_at", "TEXT NOT NULL DEFAULT ''")?;

    // Indexes on migrated columns — MUST come after ensure_column so the
    // columns exist on legacy databases that were created without them.
    conn.execute_batch(r#"
        CREATE INDEX IF NOT EXISTS idx_memories_archived    ON memories(archived);
        CREATE INDEX IF NOT EXISTS idx_memories_last_access ON memories(last_access DESC);
        CREATE INDEX IF NOT EXISTS idx_derived_source       ON derived_items(source);
        CREATE INDEX IF NOT EXISTS idx_derived_path         ON derived_items(path);
        CREATE INDEX IF NOT EXISTS idx_derived_created_at   ON derived_items(created_at DESC);
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

    ensure_fts_backfilled(conn)?;

    // NOTE: sqlite-vec virtual table (memories_vec) is created separately after
    // the extension is loaded by the caller via register_sqlite_vec().
    Ok(())
}

/// Register the sqlite-vec extension globally via sqlite3_auto_extension.
/// Must be called ONCE before any Connection::open(). Safe to call multiple times.
pub fn register_sqlite_vec() {
    SQLITE_VEC_AUTO_EXT_ONCE.call_once(|| unsafe {
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())
        ));
        if rc != rusqlite::ffi::SQLITE_OK {
            eprintln!("warning: sqlite-vec auto_extension registration failed (rc={rc})");
        }
    });
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

fn ensure_fts_backfilled(conn: &Connection) -> Result<(), MemoryError> {
    let memories_count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    if memories_count == 0 {
        return Ok(());
    }

    let fts_count: i64 = conn.query_row("SELECT COUNT(*) FROM memories_fts", [], |row| row.get(0))?;
    if fts_count > 0 {
        return Ok(());
    }

    conn.execute(
        r#"INSERT INTO memories_fts (id, path, summary, text, keywords, entities)
           SELECT
             id,
             path,
             summary,
             text,
             trim(replace(replace(replace(keywords, '[', ' '), ']', ' '), '"', ' ')),
             trim(replace(replace(replace(entities, '[', ' '), ']', ' '), '"', ' '))
           FROM memories"#,
        [],
    )?;

    Ok(())
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
pub fn upsert(conn: &mut Connection, entry: &MemoryEntry, vec_available: bool) -> Result<(), MemoryError> {
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
    let tx = conn.transaction()?;

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
/// Also records access timestamps for ACT-R base-level activation.
pub fn record_access(conn: &mut Connection, ids: &[String]) -> Result<(), MemoryError> {
    if ids.is_empty() {
        return Ok(());
    }

    let now = now_utc_iso();
    let tx = conn.transaction()?;
    for id in ids {
        tx.execute(
            "UPDATE memories
             SET access_count = access_count + 1, last_access = ?1, updated_at = ?1
             WHERE id = ?2",
            params![&now, id],
        )?;
        // Also record timestamp for ACT-R base-level activation
        tx.execute(
            "INSERT INTO access_history (memory_id, accessed_at) VALUES (?1, ?2)",
            params![id, &now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Record access timestamps for ACT-R base-level activation.
/// Called alongside record_access after each search.
pub fn record_access_history(conn: &mut Connection, ids: &[String]) -> Result<(), MemoryError> {
    if ids.is_empty() {
        return Ok(());
    }
    let now = now_utc_iso();
    let tx = conn.transaction()?;
    for id in ids {
        tx.execute(
            "INSERT INTO access_history (memory_id, accessed_at) VALUES (?1, ?2)",
            params![id, &now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Fetch access timestamps for a set of memory IDs (for ACT-R base-level activation).
/// Returns a map from memory_id -> sorted list of seconds-since-epoch (age in seconds).
pub fn get_access_times(conn: &Connection, ids: &[String]) -> Result<HashMap<String, Vec<f64>>, MemoryError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
    let sql = format!(
        "SELECT memory_id, accessed_at FROM access_history WHERE memory_id IN ({}) ORDER BY accessed_at DESC",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_vec: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params_vec.as_slice(), |row| {
        let mem_id: String = row.get(0)?;
        let at: String = row.get(1)?;
        Ok((mem_id, at))
    })?;

    let now = Utc::now();
    let mut result: HashMap<String, Vec<f64>> = HashMap::new();
    for row in rows {
        let (mem_id, at_str) = row?;
        if let Ok(dt) = at_str.parse::<DateTime<Utc>>() {
            let age_secs = (now - dt).num_seconds().max(1) as f64;
            result.entry(mem_id).or_default().push(age_secs);
        }
    }
    Ok(result)
}

// ─── DELETE ───────────────────────────────────────────────────────────────────

/// Delete a memory entry by ID from main table, FTS index, and vector table.
/// Returns true if an entry was found and deleted.
pub fn delete(conn: &mut Connection, id: &str, vec_available: bool) -> Result<bool, MemoryError> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err(MemoryError::InvalidArg("empty ID".to_string()));
    }

    let tx = conn.transaction()?;

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

        // Clean up graph edges
        tx.execute("DELETE FROM memory_edges WHERE source_id = ?1 OR target_id = ?1", params![trimmed])?;
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

// ─── Graph Operations ─────────────────────────────────────────────────────────

use crate::types::{MemoryEdge, GraphExpandResult};

/// Add or update an edge in the memory graph.
pub fn add_edge(conn: &Connection, edge: &MemoryEdge) -> Result<(), MemoryError> {
    let created = if edge.created_at.is_empty() {
        now_utc_iso()
    } else {
        normalize_utc_iso_or_now(&edge.created_at)
    };
    let valid_from = if edge.valid_from.is_empty() {
        created.clone()
    } else {
        normalize_utc_iso_or_now(&edge.valid_from)
    };
    let meta_str = serde_json::to_string(&edge.metadata)
        .unwrap_or_else(|_| "{}".to_string());

    conn.execute(
        r#"INSERT INTO memory_edges (source_id, target_id, relation, weight, metadata, created_at, valid_from, valid_to)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
           ON CONFLICT(source_id, target_id, relation)
           DO UPDATE SET weight = ?4, metadata = ?5, created_at = ?6, valid_from = ?7, valid_to = ?8"#,
        params![edge.source_id, edge.target_id, edge.relation, edge.weight, meta_str, created, valid_from, edge.valid_to],
    )?;
    Ok(())
}

/// Remove an edge from the memory graph.
pub fn remove_edge(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    relation: &str,
) -> Result<bool, MemoryError> {
    let count = conn.execute(
        "DELETE FROM memory_edges WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
        params![source_id, target_id, relation],
    )?;
    Ok(count > 0)
}

/// Get all edges connected to a memory ID.
/// direction: "outgoing" (source_id match), "incoming" (target_id match), or "both"
pub fn get_edges(
    conn: &Connection,
    memory_id: &str,
    direction: &str,
    relation_filter: Option<&str>,
) -> Result<Vec<MemoryEdge>, MemoryError> {
    let (sql, id_param) = match direction {
        "incoming" => (
            "SELECT source_id, target_id, relation, weight, metadata, created_at, valid_from, valid_to FROM memory_edges WHERE target_id = ?1
             AND (valid_to IS NULL OR valid_to > datetime('now'))",
            memory_id,
        ),
        "outgoing" => (
            "SELECT source_id, target_id, relation, weight, metadata, created_at, valid_from, valid_to FROM memory_edges WHERE source_id = ?1
             AND (valid_to IS NULL OR valid_to > datetime('now'))",
            memory_id,
        ),
        _ => (
            "SELECT source_id, target_id, relation, weight, metadata, created_at, valid_from, valid_to FROM memory_edges WHERE source_id = ?1 OR target_id = ?1
             AND (valid_to IS NULL OR valid_to > datetime('now'))",
            memory_id,
        ),
    };

    let full_sql = if let Some(rel) = relation_filter {
        format!("{} AND relation = '{}'", sql, rel.replace('\'', "''"))
    } else {
        sql.to_string()
    };

    let mut stmt = conn.prepare(&full_sql)?;
    let edges = stmt.query_map(params![id_param], |row| {
        let meta_str: String = row.get(4)?;
        let metadata = serde_json::from_str(&meta_str).unwrap_or_default();
        Ok(MemoryEdge {
            source_id: row.get(0)?,
            target_id: row.get(1)?,
            relation: row.get(2)?,
            weight: row.get(3)?,
            metadata,
            created_at: row.get(5)?,
            valid_from: row.get(6)?,
            valid_to: row.get(7)?,
        })
    })?.filter_map(|r| r.ok()).collect();

    Ok(edges)
}

/// Count the number of 'contradicts' edges for a given memory ID.
/// Used by surprise scoring to detect controversial/surprising memories.
pub fn get_contradiction_count(conn: &Connection, memory_id: &str) -> Result<u32, MemoryError> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM memory_edges WHERE (source_id = ?1 OR target_id = ?1) AND relation = 'contradicts' AND (valid_to IS NULL OR valid_to > datetime('now'))",
        params![memory_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Count how many memories share the same topic as the given entry.
/// Used by surprise scoring for topic novelty.
pub fn count_same_topic(conn: &Connection, topic: &str) -> Result<u32, MemoryError> {
    if topic.is_empty() {
        return Ok(0);
    }
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE topic = ?1 AND archived = 0",
        params![topic],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Get the average importance across all non-archived memories.
/// Used by surprise scoring.
pub fn avg_importance(conn: &Connection) -> Result<f64, MemoryError> {
    let avg: f64 = conn.query_row(
        "SELECT COALESCE(AVG(importance), 0.7) FROM memories WHERE archived = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(avg)
}

/// BFS graph expansion from seed memory IDs.
/// Returns entries found within `max_hops` of any seed, plus the edges traversed.
pub fn graph_expand(
    conn: &Connection,
    seed_ids: &[String],
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Result<GraphExpandResult, MemoryError> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut visited: HashSet<String> = HashSet::new();
    let mut distances: HashMap<String, u32> = HashMap::new();
    let mut all_edges: Vec<MemoryEdge> = Vec::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();

    // Initialize with seeds at distance 0
    for id in seed_ids {
        if visited.insert(id.clone()) {
            distances.insert(id.clone(), 0);
            queue.push_back((id.clone(), 0));
        }
    }

    // BFS
    while let Some((current_id, depth)) = queue.pop_front() {
        if depth >= max_hops {
            continue;
        }

        let edges = get_edges(conn, &current_id, "both", relation_filter)?;
        for edge in &edges {
            let neighbor = if edge.source_id == current_id {
                &edge.target_id
            } else {
                &edge.source_id
            };

            if visited.insert(neighbor.clone()) {
                let new_depth = depth + 1;
                distances.insert(neighbor.clone(), new_depth);
                queue.push_back((neighbor.clone(), new_depth));
            }
        }
        all_edges.extend(edges);
    }

    // Fetch all discovered entries (exclude seeds — caller already has those)
    let non_seed_ids: Vec<String> = distances.keys()
        .filter(|id| !seed_ids.contains(id))
        .cloned()
        .collect();

    let entries = if non_seed_ids.is_empty() {
        Vec::new()
    } else {
        let map = fetch_by_ids(conn, &non_seed_ids, false)?;
        map.into_values().collect()
    };

    // Deduplicate edges
    all_edges.sort_by(|a, b| {
        (&a.source_id, &a.target_id, &a.relation)
            .cmp(&(&b.source_id, &b.target_id, &b.relation))
    });
    all_edges.dedup_by(|a, b| {
        a.source_id == b.source_id && a.target_id == b.target_id && a.relation == b.relation
    });

    Ok(GraphExpandResult {
        entries,
        edges: all_edges,
        distances,
    })
}

/// Remove all edges referencing a memory ID (both as source and target).
/// Call this when deleting a memory entry to keep the graph consistent.
pub fn remove_edges_for_memory(conn: &Connection, memory_id: &str) -> Result<usize, MemoryError> {
    let count = conn.execute(
        "DELETE FROM memory_edges WHERE source_id = ?1 OR target_id = ?1",
        params![memory_id],
    )?;
    Ok(count)
}

// ─── Hard State Operations ─────────────────────────────────────────────────────

/// Set a key-value pair in the hard_state table. INSERT OR UPDATE with version bump.
pub fn set_state(
    conn: &Connection,
    namespace: &str,
    key: &str,
    value_json: &str,
) -> Result<u32, MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "INSERT INTO hard_state (namespace, key, value_json, version, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, ?4, ?4)
         ON CONFLICT(namespace, key) DO UPDATE SET
           value_json = excluded.value_json,
           version = hard_state.version + 1,
           updated_at = excluded.updated_at",
        params![namespace, key, value_json, &now],
    )?;
    let version: u32 = conn.query_row(
        "SELECT version FROM hard_state WHERE namespace = ?1 AND key = ?2",
        params![namespace, key],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// Get a key-value pair from the hard_state table.
pub fn get_state(
    conn: &Connection,
    namespace: &str,
    key: &str,
) -> Result<Option<(String, u32)>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT value_json, version FROM hard_state WHERE namespace = ?1 AND key = ?2"
    )?;
    let result = stmt.query_row(params![namespace, key], |row| {
        let val: String = row.get(0)?;
        let ver: u32 = row.get(1)?;
        Ok((val, ver))
    });
    match result {
        Ok(pair) => Ok(Some(pair)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemoryError::from(e)),
    }
}

/// Save a derived item (causal extraction, distilled rule, etc.)
pub fn save_derived(
    conn: &Connection,
    text: &str,
    path: &str,
    summary: &str,
    importance: f64,
    source: &str,
    scope: &str,
    metadata: &serde_json::Value,
) -> Result<String, MemoryError> {
    let id = Uuid::new_v4().to_string();
    let now = now_utc_iso();
    let metadata_json = serde_json::to_string(metadata)?;

    conn.execute(
        r#"INSERT INTO derived_items (id, text, path, summary, importance, source, scope, metadata, created_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
        params![id, text, path, summary, importance, source, scope, metadata_json, now],
    )?;

    Ok(id)
}

/// Count derived items by source and path prefix.
pub fn count_derived_by_source(
    conn: &Connection,
    source: &str,
    path_prefix: &str,
) -> Result<u64, MemoryError> {
    let like_pattern = format!("{}%", path_prefix);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM derived_items WHERE source = ?1 AND path LIKE ?2",
        params![source, like_pattern],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// List derived items by source and path prefix.
pub fn list_derived_by_source(
    conn: &Connection,
    source: &str,
    path_prefix: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, MemoryError> {
    let like_pattern = format!("{}%", path_prefix);
    let mut stmt = conn.prepare(
        "SELECT id, text, path, summary, importance, source, scope, metadata, created_at
         FROM derived_items
         WHERE source = ?1 AND path LIKE ?2
         ORDER BY created_at DESC
         LIMIT ?3"
    )?;

    let rows = stmt.query_map(params![source, like_pattern, limit as i64], |row| {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "text": row.get::<_, String>(1)?,
            "path": row.get::<_, String>(2)?,
            "summary": row.get::<_, String>(3)?,
            "importance": row.get::<_, f64>(4)?,
            "source": row.get::<_, String>(5)?,
            "scope": row.get::<_, String>(6)?,
            "metadata": row.get::<_, String>(7)?,
            "created_at": row.get::<_, String>(8)?,
        }))
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Archive a memory entry (set archived = 1).
pub fn archive_memory(conn: &Connection, id: &str) -> Result<bool, MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "UPDATE memories SET archived = 1, updated_at = ?1 WHERE id = ?2 AND archived = 0",
        params![now, id],
    )?;
    Ok(conn.changes() > 0)
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
        let mut conn = make_conn();
        let e = make_entry("abc", "Rust is a systems programming language");
        upsert(&mut conn, &e, false).unwrap();

        let results = search_fts(&conn, "systems programming", 5, false).unwrap();
        assert!(results.contains_key("abc"), "expected 'abc' in FTS results");
    }

    #[test]
    fn upsert_idempotent() {
        let mut conn = make_conn();
        let mut e = make_entry("dup", "first text");
        upsert(&mut conn, &e, false).unwrap();
        e.text = "updated text".into();
        upsert(&mut conn, &e, false).unwrap();

        let results = search_fts(&conn, "updated", 5, false).unwrap();
        assert!(results.contains_key("dup"));
    }

    #[test]
    fn search_vec_knn_with_k_constraint() {
        let mut conn = make_conn();
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
        upsert(&mut conn, &e, true).unwrap();

        let query = vec![0.1_f32; 1024];
        let results = search_vec(&conn, &query, 3, false).unwrap();
        assert!(results.contains_key("vec-1"));
    }

    #[test]
    fn delete_existing() {
        let mut conn = make_conn();
        let e = make_entry("del-1", "to be deleted");
        upsert(&mut conn, &e, false).unwrap();

        let deleted = delete(&mut conn, "del-1", false).unwrap();
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
        let mut conn = make_conn();
        let deleted = delete(&mut conn, "nonexistent-id", false).unwrap();
        assert!(!deleted, "should return false for non-existent entry");
    }

    #[test]
    fn stats_aggregation() {
        let mut conn = make_conn();

        let mut e1 = make_entry("s1", "fact entry");
        e1.scope = "general".into();
        e1.category = "fact".into();
        e1.path = "/project/alpha".into();
        upsert(&mut conn, &e1, false).unwrap();

        let mut e2 = make_entry("s2", "decision entry");
        e2.scope = "project".into();
        e2.category = "decision".into();
        e2.path = "/project/beta".into();
        upsert(&mut conn, &e2, false).unwrap();

        let mut e3 = make_entry("s3", "user preference");
        e3.scope = "user".into();
        e3.category = "preference".into();
        e3.path = "/user/settings".into();
        upsert(&mut conn, &e3, false).unwrap();

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

    #[test]
    fn graph_add_and_get_edges() {
        let mut conn = make_conn();
        let e1 = make_entry("g1", "cause event");
        let e2 = make_entry("g2", "effect event");
        upsert(&mut conn, &e1, false).unwrap();
        upsert(&mut conn, &e2, false).unwrap();

        let edge = MemoryEdge {
            source_id: "g1".into(),
            target_id: "g2".into(),
            relation: "causes".into(),
            weight: 0.9,
            metadata: serde_json::json!({}),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        };
        add_edge(&conn, &edge).unwrap();

        let out = get_edges(&conn, "g1", "outgoing", None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_id, "g2");
        assert_eq!(out[0].relation, "causes");

        let inc = get_edges(&conn, "g2", "incoming", None).unwrap();
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].source_id, "g1");
    }

    #[test]
    fn graph_expand_bfs() {
        let mut conn = make_conn();
        // Create chain: a -> b -> c
        for id in &["a", "b", "c", "d"] {
            upsert(&mut conn, &make_entry(id, &format!("node {}", id)), false).unwrap();
        }
        add_edge(&conn, &MemoryEdge {
            source_id: "a".into(), target_id: "b".into(),
            relation: "follows".into(), weight: 1.0,
            metadata: serde_json::json!({}), created_at: String::new(),
            valid_from: String::new(), valid_to: None,
        }).unwrap();
        add_edge(&conn, &MemoryEdge {
            source_id: "b".into(), target_id: "c".into(),
            relation: "follows".into(), weight: 1.0,
            metadata: serde_json::json!({}), created_at: String::new(),
            valid_from: String::new(), valid_to: None,
        }).unwrap();
        // d is disconnected

        // Expand 1 hop from "a"
        let r1 = graph_expand(&conn, &["a".into()], 1, None).unwrap();
        assert_eq!(r1.entries.len(), 1); // should find b
        assert!(r1.distances.contains_key("b"));
        assert!(!r1.distances.contains_key("c")); // c is 2 hops

        // Expand 2 hops from "a"
        let r2 = graph_expand(&conn, &["a".into()], 2, None).unwrap();
        assert_eq!(r2.entries.len(), 2); // b and c
        assert!(r2.distances.contains_key("c"));
        assert!(!r2.distances.contains_key("d")); // d is disconnected
    }

    #[test]
    fn delete_cascades_edges() {
        let mut conn = make_conn();
        let e1 = make_entry("del-e1", "source");
        let e2 = make_entry("del-e2", "target");
        upsert(&mut conn, &e1, false).unwrap();
        upsert(&mut conn, &e2, false).unwrap();
        add_edge(&conn, &MemoryEdge {
            source_id: "del-e1".into(), target_id: "del-e2".into(),
            relation: "causes".into(), weight: 1.0,
            metadata: serde_json::json!({}), created_at: String::new(),
            valid_from: String::new(), valid_to: None,
        }).unwrap();

        delete(&mut conn, "del-e1", false).unwrap();
        let edges = get_edges(&conn, "del-e2", "both", None).unwrap();
        assert!(edges.is_empty(), "edges should be cleaned up on delete");
    }
}
