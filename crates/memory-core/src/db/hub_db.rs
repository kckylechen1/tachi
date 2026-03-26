use rusqlite::{params, Connection};

use crate::error::MemoryError;
use crate::hub::HubCapability;

use super::common::now_utc_iso;

pub fn hub_upsert(conn: &Connection, cap: &HubCapability) -> Result<(), MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "INSERT INTO hub_capabilities (id, type, name, version, description, definition, enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(id) DO UPDATE SET
           type = excluded.type,
           name = excluded.name,
           version = excluded.version,
           description = excluded.description,
           definition = excluded.definition,
           enabled = excluded.enabled,
           updated_at = excluded.updated_at",
        params![
            &cap.id,
            &cap.cap_type,
            &cap.name,
            cap.version,
            &cap.description,
            &cap.definition,
            cap.enabled as i32,
            &now,
        ],
    )?;
    Ok(())
}

/// Get a single hub capability by ID.
pub fn hub_get(conn: &Connection, id: &str) -> Result<Option<HubCapability>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, version, description, definition, enabled,
                uses, successes, failures, avg_rating, last_used, created_at, updated_at
         FROM hub_capabilities WHERE id = ?1",
    )?;
    let result = stmt.query_row(params![id], |row| Ok(hub_cap_from_row(row)));
    match result {
        Ok(cap) => Ok(Some(cap)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(MemoryError::from(e)),
    }
}

/// List hub capabilities, optionally filtered by type and enabled status.
pub fn hub_list(
    conn: &Connection,
    cap_type: Option<&str>,
    enabled_only: bool,
) -> Result<Vec<HubCapability>, MemoryError> {
    let mut sql = String::from(
        "SELECT id, type, name, version, description, definition, enabled,
                uses, successes, failures, avg_rating, last_used, created_at, updated_at
         FROM hub_capabilities WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(t) = cap_type {
        sql.push_str(" AND type = ?");
        param_values.push(Box::new(t.to_string()));
    }
    if enabled_only {
        sql.push_str(" AND enabled = 1");
    }
    sql.push_str(" ORDER BY name ASC");

    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| Ok(hub_cap_from_row(row)))?;

    let mut caps = Vec::new();
    for row in rows {
        caps.push(row?);
    }
    Ok(caps)
}

/// Search hub capabilities by name/description using LIKE.
pub fn hub_search(
    conn: &Connection,
    query: &str,
    cap_type: Option<&str>,
) -> Result<Vec<HubCapability>, MemoryError> {
    let pattern = format!("%{}%", query);
    let mut sql = String::from(
        "SELECT id, type, name, version, description, definition, enabled,
                uses, successes, failures, avg_rating, last_used, created_at, updated_at
         FROM hub_capabilities
         WHERE (name LIKE ?1 OR description LIKE ?1)",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(pattern)];

    if let Some(t) = cap_type {
        sql.push_str(" AND type = ?2");
        param_values.push(Box::new(t.to_string()));
    }
    sql.push_str(" ORDER BY uses DESC, name ASC");

    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| Ok(hub_cap_from_row(row)))?;

    let mut caps = Vec::new();
    for row in rows {
        caps.push(row?);
    }
    Ok(caps)
}

/// Enable or disable a hub capability. Returns true if the row was found.
pub fn hub_set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<bool, MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "UPDATE hub_capabilities SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
        params![enabled as i32, &now, id],
    )?;
    Ok(conn.changes() > 0)
}

/// Record feedback for a hub capability invocation.
pub fn hub_record_feedback(
    conn: &Connection,
    id: &str,
    success: bool,
    rating: Option<f64>,
) -> Result<(), MemoryError> {
    let now = now_utc_iso();
    // Update counters
    conn.execute(
        "UPDATE hub_capabilities SET
           uses = uses + 1,
           successes = successes + ?1,
           failures = failures + ?2,
           last_used = ?3,
           updated_at = ?3
         WHERE id = ?4",
        params![success as i32, (!success) as i32, &now, id,],
    )?;

    // Update running average rating if provided
    if let Some(r) = rating {
        conn.execute(
            "UPDATE hub_capabilities SET
               avg_rating = CASE
                 WHEN uses <= 1 THEN ?1
                 ELSE avg_rating + (?1 - avg_rating) / uses
               END
             WHERE id = ?2",
            params![r, id],
        )?;
    }

    Ok(())
}

/// Delete a hub capability. Returns true if found and deleted.
pub fn hub_delete(conn: &Connection, id: &str) -> Result<bool, MemoryError> {
    conn.execute("DELETE FROM hub_capabilities WHERE id = ?1", params![id])?;
    Ok(conn.changes() > 0)
}

/// Helper: build HubCapability from a row (tolerant of unexpected data).
fn hub_cap_from_row(row: &rusqlite::Row) -> HubCapability {
    HubCapability {
        id: row.get(0).unwrap_or_default(),
        cap_type: row.get(1).unwrap_or_default(),
        name: row.get(2).unwrap_or_default(),
        version: row.get::<_, u32>(3).unwrap_or(1),
        description: row.get(4).unwrap_or_default(),
        definition: row.get(5).unwrap_or_default(),
        enabled: row.get::<_, i32>(6).unwrap_or(1) != 0,
        uses: row.get::<_, i64>(7).unwrap_or(0) as u64,
        successes: row.get::<_, i64>(8).unwrap_or(0) as u64,
        failures: row.get::<_, i64>(9).unwrap_or(0) as u64,
        avg_rating: row.get(10).unwrap_or(0.0),
        last_used: row.get(11).unwrap_or(None),
        created_at: row.get(12).unwrap_or_default(),
        updated_at: row.get(13).unwrap_or_default(),
    }
}
