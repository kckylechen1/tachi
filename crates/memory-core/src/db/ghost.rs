use rusqlite::{params, Connection, OptionalExtension};

use crate::error::MemoryError;

use super::common::now_utc_iso;

fn parse_json_text(text: &str, fallback: serde_json::Value) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or(fallback)
}

/// Persist one Ghost message and advance topic counters atomically.
pub fn ghost_publish_message(
    conn: &mut Connection,
    id: &str,
    topic: &str,
    payload_json: &str,
    publisher: &str,
    timestamp: &str,
) -> Result<u64, MemoryError> {
    let tx = conn.unchecked_transaction()?;
    let current_total: i64 = tx
        .query_row(
            "SELECT total_published FROM ghost_topics WHERE topic = ?1",
            params![topic],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let next_index = current_total.max(0) + 1;

    tx.execute(
        "INSERT INTO ghost_messages
         (id, topic, topic_index, payload, publisher, timestamp, promoted, importance, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0.5, ?6)",
        params![id, topic, next_index, payload_json, publisher, timestamp],
    )?;

    tx.execute(
        "INSERT INTO ghost_topics (topic, total_published, last_message_time, last_publisher, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(topic) DO UPDATE SET
           total_published = excluded.total_published,
           last_message_time = excluded.last_message_time,
           last_publisher = excluded.last_publisher,
           updated_at = excluded.updated_at",
        params![topic, next_index, timestamp, publisher, timestamp],
    )?;

    tx.commit()?;
    Ok(next_index as u64)
}

/// Fetch topic messages newer than `after_index`, ordered by topic_index asc.
pub fn ghost_fetch_messages_since(
    conn: &Connection,
    topic: &str,
    after_index: u64,
    limit: usize,
) -> Result<Vec<serde_json::Value>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, topic, topic_index, payload, publisher, timestamp, promoted, importance
         FROM ghost_messages
         WHERE topic = ?1 AND topic_index > ?2
         ORDER BY topic_index ASC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![topic, after_index as i64, limit as i64], |row| {
        let payload_text: String = row.get(3)?;
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "topic": row.get::<_, String>(1)?,
            "topic_index": row.get::<_, i64>(2)?,
            "payload": parse_json_text(&payload_text, serde_json::json!({})),
            "publisher": row.get::<_, String>(4)?,
            "timestamp": row.get::<_, String>(5)?,
            "promoted": row.get::<_, i32>(6)? != 0,
            "importance": row.get::<_, f64>(7)?,
        }))
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Upsert one agent-topic subscription.
pub fn ghost_upsert_subscription(
    conn: &Connection,
    agent_id: &str,
    topic: &str,
) -> Result<(), MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "INSERT INTO ghost_subscriptions (agent_id, topic, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(agent_id, topic) DO UPDATE SET
           updated_at = excluded.updated_at",
        params![agent_id, topic, &now],
    )?;
    Ok(())
}

/// Read cursor for one agent-topic. Missing cursor => 0.
pub fn ghost_get_cursor(
    conn: &Connection,
    agent_id: &str,
    topic: &str,
) -> Result<u64, MemoryError> {
    let cursor: Option<i64> = conn
        .query_row(
            "SELECT last_seen_index FROM ghost_cursors WHERE agent_id = ?1 AND topic = ?2",
            params![agent_id, topic],
            |row| row.get(0),
        )
        .optional()?;
    Ok(cursor.unwrap_or(0).max(0) as u64)
}

/// Upsert cursor for one agent-topic.
pub fn ghost_set_cursor(
    conn: &Connection,
    agent_id: &str,
    topic: &str,
    last_seen_index: u64,
) -> Result<(), MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "INSERT INTO ghost_cursors (agent_id, topic, last_seen_index, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id, topic) DO UPDATE SET
           last_seen_index = excluded.last_seen_index,
           updated_at = excluded.updated_at",
        params![agent_id, topic, last_seen_index as i64, &now],
    )?;
    Ok(())
}

/// Resolve one message id to `(topic, topic_index)`.
pub fn ghost_get_message_topic_index(
    conn: &Connection,
    message_id: &str,
) -> Result<Option<(String, u64)>, MemoryError> {
    let result: Option<(String, i64)> = conn
        .query_row(
            "SELECT topic, topic_index FROM ghost_messages WHERE id = ?1",
            params![message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(result.map(|(topic, idx)| (topic, idx.max(0) as u64)))
}

/// Read total published messages for a topic.
pub fn ghost_get_topic_total(conn: &Connection, topic: &str) -> Result<u64, MemoryError> {
    let total: Option<i64> = conn
        .query_row(
            "SELECT total_published FROM ghost_topics WHERE topic = ?1",
            params![topic],
            |row| row.get(0),
        )
        .optional()?;
    Ok(total.unwrap_or(0).max(0) as u64)
}

/// List active topics with aggregate metadata.
pub fn ghost_list_topics(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<serde_json::Value>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT topic, total_published, last_message_time, last_publisher
         FROM ghost_topics
         ORDER BY COALESCE(last_message_time, '') DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(serde_json::json!({
            "topic": row.get::<_, String>(0)?,
            "total_published": row.get::<_, i64>(1)?,
            "last_message_time": row.get::<_, Option<String>>(2)?,
            "last_publisher": row.get::<_, Option<String>>(3)?,
        }))
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Insert one reflection row.
pub fn ghost_insert_reflection(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    topic: Option<&str>,
    summary: &str,
    metadata_json: &str,
    timestamp: &str,
) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO ghost_reflections (id, agent_id, topic, summary, metadata, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, agent_id, topic, summary, metadata_json, timestamp],
    )?;
    Ok(())
}

/// Fetch one message payload by id.
pub fn ghost_get_message(
    conn: &Connection,
    message_id: &str,
) -> Result<Option<serde_json::Value>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, topic, topic_index, payload, publisher, timestamp, promoted, importance
         FROM ghost_messages
         WHERE id = ?1",
    )?;
    let row = stmt
        .query_row(params![message_id], |row| {
            let payload_text: String = row.get(3)?;
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "topic": row.get::<_, String>(1)?,
                "topic_index": row.get::<_, i64>(2)?,
                "payload": parse_json_text(&payload_text, serde_json::json!({})),
                "publisher": row.get::<_, String>(4)?,
                "timestamp": row.get::<_, String>(5)?,
                "promoted": row.get::<_, i32>(6)? != 0,
                "importance": row.get::<_, f64>(7)?,
            }))
        })
        .optional()?;
    Ok(row)
}

/// Mark one message as promoted and optionally update promoted importance.
pub fn ghost_mark_message_promoted(
    conn: &Connection,
    message_id: &str,
    importance: Option<f64>,
) -> Result<bool, MemoryError> {
    match importance {
        Some(value) => {
            conn.execute(
                "UPDATE ghost_messages
                 SET promoted = 1, importance = ?2
                 WHERE id = ?1",
                params![message_id, value],
            )?;
        }
        None => {
            conn.execute(
                "UPDATE ghost_messages
                 SET promoted = 1
                 WHERE id = ?1",
                params![message_id],
            )?;
        }
    }
    Ok(conn.changes() > 0)
}
