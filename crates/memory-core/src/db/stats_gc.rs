use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::error::MemoryError;

// ─── GC (Garbage Collection) ──────────────────────────────────────────────────

/// Retention-based cleanup of growing tables.
/// Returns a summary of how many rows were deleted from each table.
pub fn gc_tables(conn: &mut Connection) -> Result<serde_json::Value, MemoryError> {
    let tx = conn.transaction()?;

    // 1. access_history: retain latest 256 entries per memory_id, delete rest
    let ah_deleted: usize = tx.execute(
        "DELETE FROM access_history
         WHERE rowid IN (
             SELECT rowid FROM (
                 SELECT rowid,
                        ROW_NUMBER() OVER (
                            PARTITION BY memory_id
                            ORDER BY accessed_at DESC
                        ) AS rn
                 FROM access_history
             ) ranked
             WHERE rn > 256
         )",
        [],
    )?;

    // 2. processed_events: delete older than 30 days
    let pe_deleted: usize = tx.execute(
        "DELETE FROM processed_events
         WHERE created_at < STRFTIME('%Y-%m-%dT%H:%M:%fZ', 'now', '-30 days')",
        [],
    )?;

    // 3. audit_log: delete older than 30 days OR keep only latest 100k
    let al_deleted: usize = tx.execute(
        "DELETE FROM audit_log
         WHERE created_at < STRFTIME('%Y-%m-%dT%H:%M:%fZ', 'now', '-30 days')",
        [],
    )?;
    // Also cap at 100k total
    let al_cap_deleted: usize = tx.execute(
        "DELETE FROM audit_log WHERE id NOT IN (
            SELECT id FROM audit_log ORDER BY id DESC LIMIT 100000
        )",
        [],
    )?;

    // 4. agent_known_state: delete older than 90 days
    let aks_deleted: usize = tx.execute(
        "DELETE FROM agent_known_state
         WHERE synced_at < STRFTIME('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')",
        [],
    )?;

    // 5. Orphaned access_history (memory was deleted but history remained)
    let orphan_deleted: usize = tx.execute(
        "DELETE FROM access_history WHERE memory_id NOT IN (SELECT id FROM memories)",
        [],
    )?;

    // 6. Orphaned agent_known_state (memory was deleted but known-state remained)
    let orphan_aks_deleted: usize = tx.execute(
        "DELETE FROM agent_known_state WHERE memory_id NOT IN (SELECT id FROM memories)",
        [],
    )?;

    tx.commit()?;

    Ok(serde_json::json!({
        "access_history_pruned": ah_deleted,
        "processed_events_pruned": pe_deleted,
        "audit_log_pruned": al_deleted + al_cap_deleted,
        "agent_known_state_pruned": aks_deleted,
        "orphaned_access_history": orphan_deleted,
        "orphaned_agent_known_state": orphan_aks_deleted,
    }))
}

// ─── AUTO-ARCHIVE STALE MEMORIES ──────────────────────────────────────────────

/// Archive low-importance memories that haven't been accessed in `stale_days`.
/// Returns the number of memories archived.
pub fn archive_stale_memories(conn: &Connection, stale_days: u32) -> Result<u64, MemoryError> {
    // Archive low-importance memories not accessed in N days
    let affected1 = conn.execute(
        "UPDATE memories SET archived = 1
         WHERE archived = 0
           AND last_access IS NOT NULL
           AND last_access < datetime('now', '-' || ?1 || ' days')
           AND importance < 0.5",
        params![stale_days],
    )?;

    // Archive very low importance memories never accessed and older than N days
    let affected2 = conn.execute(
        "UPDATE memories SET archived = 1
         WHERE archived = 0
           AND last_access IS NULL
           AND timestamp < datetime('now', '-' || ?1 || ' days')
           AND importance < 0.3",
        params![stale_days],
    )?;

    Ok((affected1 + affected2) as u64)
}

// ─── STATS ────────────────────────────────────────────────────────────────────

/// Get aggregate statistics about the memory store.
pub fn stats(
    conn: &Connection,
    include_archived: bool,
) -> Result<crate::types::StatsResult, MemoryError> {
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

    let mut stmt = conn.prepare("SELECT path FROM memories WHERE (?1 = 1 OR archived = 0)")?;
    let rows = stmt.query_map(params![include_archived as i64], |row| {
        row.get::<_, String>(0)
    })?;
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
