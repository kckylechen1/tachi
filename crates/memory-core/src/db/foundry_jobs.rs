use rusqlite::{params, Connection};
use serde_json;

use crate::error::MemoryError;
use crate::foundry::{FoundryJobKind, FoundryJobSpec, FoundryJobStatus, FoundryModelLane};

/// A persisted Foundry job row including dispatch context.
#[derive(Debug, Clone)]
pub struct PersistedFoundryJob {
    pub spec: FoundryJobSpec,
    pub target_db: String,
    pub named_project: Option<String>,
    pub path_prefix: String,
    pub memory_ids: Vec<String>,
}

/// Insert a new Foundry job (or replace if ID already exists).
pub fn insert_foundry_job(conn: &Connection, job: &PersistedFoundryJob) -> Result<(), MemoryError> {
    let now = chrono::Utc::now().to_rfc3339();
    let kind_str = serde_json::to_string(&job.spec.kind)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    let lane_str = serde_json::to_string(&job.spec.lane)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    let status_str = serde_json::to_string(&job.spec.status)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();

    conn.execute(
        "INSERT OR REPLACE INTO foundry_jobs
         (id, kind, lane, status, target_db, named_project, path_prefix, memory_ids,
          target_agent_id, requested_by, evidence_count, goal_count, metadata,
          created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            job.spec.id,
            kind_str,
            lane_str,
            status_str,
            job.target_db,
            job.named_project,
            job.path_prefix,
            serde_json::to_string(&job.memory_ids).unwrap_or_else(|_| "[]".to_string()),
            job.spec.target_agent_id,
            job.spec.requested_by,
            job.spec.evidence_count as i64,
            job.spec.goal_count as i64,
            job.spec.metadata.to_string(),
            if job.spec.created_at.is_empty() { &now } else { &job.spec.created_at },
            now,
        ],
    )?;
    Ok(())
}

/// Update job status (and updated_at).
pub fn update_foundry_job_status(
    conn: &Connection,
    id: &str,
    status: &str,
) -> Result<(), MemoryError> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE foundry_jobs SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, now, id],
    )?;
    Ok(())
}

/// Load all jobs with status 'queued' or 'running' (for startup replay).
pub fn load_pending_foundry_jobs(
    conn: &Connection,
) -> Result<Vec<PersistedFoundryJob>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, lane, status, target_db, named_project, path_prefix, memory_ids,
                target_agent_id, requested_by, evidence_count, goal_count, metadata, created_at
         FROM foundry_jobs
         WHERE status IN ('queued', 'running')
         ORDER BY created_at ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        let kind_str: String = row.get(1)?;
        let lane_str: String = row.get(2)?;
        let status_str: String = row.get(3)?;
        let memory_ids_str: String = row.get(7)?;
        let metadata_str: String = row.get(12)?;

        Ok(PersistedFoundryJob {
            spec: FoundryJobSpec {
                id: row.get(0)?,
                kind: serde_json::from_str(&format!("\"{}\"", kind_str))
                    .unwrap_or(FoundryJobKind::ForgetSweep),
                lane: serde_json::from_str(&format!("\"{}\"", lane_str))
                    .unwrap_or(FoundryModelLane::Distill),
                status: serde_json::from_str(&format!("\"{}\"", status_str))
                    .unwrap_or(FoundryJobStatus::Queued),
                target_agent_id: row.get(8)?,
                requested_by: row.get(9)?,
                created_at: row.get(13)?,
                evidence_count: row.get::<_, i64>(10).unwrap_or(0) as usize,
                goal_count: row.get::<_, i64>(11).unwrap_or(1) as usize,
                metadata: serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({})),
            },
            target_db: row.get(4)?,
            named_project: row.get(5)?,
            path_prefix: row.get(6)?,
            memory_ids: serde_json::from_str(&memory_ids_str).unwrap_or_default(),
        })
    })?;

    let mut jobs = Vec::new();
    for job in rows.flatten() {
        jobs.push(job);
    }
    Ok(jobs)
}

/// Delete completed/failed/skipped jobs older than `days` days.
pub fn gc_foundry_jobs(conn: &Connection, days: i64) -> Result<usize, MemoryError> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let deleted = conn.execute(
        "DELETE FROM foundry_jobs WHERE status IN ('completed', 'failed', 'skipped') AND created_at < ?1",
        params![cutoff],
    )?;
    Ok(deleted)
}
