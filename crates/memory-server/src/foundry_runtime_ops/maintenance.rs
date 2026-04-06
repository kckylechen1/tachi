use super::capture::*;
use super::helpers::*;
use super::*;
use std::sync::atomic::Ordering;

fn foundry_requested_by(server: &MemoryServer) -> Option<String> {
    read_or_recover(&server.agent_profile, "agent_profile")
        .as_ref()
        .map(|profile| profile.agent_id.clone())
}

fn foundry_job_lane(kind: &memory_core::FoundryJobKind) -> memory_core::FoundryModelLane {
    match kind {
        memory_core::FoundryJobKind::MemoryRerank => memory_core::FoundryModelLane::Rerank,
        memory_core::FoundryJobKind::MemoryDistill => memory_core::FoundryModelLane::Distill,
        memory_core::FoundryJobKind::ForgetSweep => memory_core::FoundryModelLane::Distill,
        _ => memory_core::FoundryModelLane::Reasoning,
    }
}

fn foundry_worker_name(kind: &memory_core::FoundryJobKind) -> &'static str {
    match kind {
        memory_core::FoundryJobKind::MemoryRerank => "foundry_rerank",
        memory_core::FoundryJobKind::MemoryDistill => "foundry_distill",
        memory_core::FoundryJobKind::ForgetSweep => "foundry_forget",
        _ => "foundry",
    }
}

pub(super) fn foundry_job_label(kind: &memory_core::FoundryJobKind) -> &'static str {
    match kind {
        memory_core::FoundryJobKind::MemoryRerank => "memory_rerank",
        memory_core::FoundryJobKind::MemoryDistill => "memory_distill",
        memory_core::FoundryJobKind::ForgetSweep => "forget_sweep",
        memory_core::FoundryJobKind::SessionIngest => "session_ingest",
        memory_core::FoundryJobKind::MemoryEnrichment => "memory_enrichment",
        memory_core::FoundryJobKind::SkillEvolution => "skill_evolution",
        memory_core::FoundryJobKind::AgentEvolution => "agent_evolution",
        memory_core::FoundryJobKind::ProfileProjection => "profile_projection",
    }
}

fn build_foundry_maintenance_job(
    server: &MemoryServer,
    kind: memory_core::FoundryJobKind,
    agent_id: &str,
    path_prefix: &str,
    memory_ids: &[String],
    metadata: serde_json::Value,
) -> memory_core::FoundryJobSpec {
    memory_core::FoundryJobSpec {
        id: format!("foundry-job:{}", uuid::Uuid::new_v4()),
        kind: kind.clone(),
        lane: foundry_job_lane(&kind),
        status: memory_core::FoundryJobStatus::Queued,
        target_agent_id: Some(agent_id.to_string()),
        requested_by: foundry_requested_by(server),
        created_at: Utc::now().to_rfc3339(),
        evidence_count: memory_ids.len(),
        goal_count: 1,
        metadata: json!({
            "path_prefix": path_prefix,
            "memory_ids": memory_ids,
            "job": metadata,
        }),
    }
}

pub(super) fn enqueue_capture_maintenance_jobs(
    server: &MemoryServer,
    target_db: DbScope,
    named_project: Option<String>,
    agent_id: &str,
    path_prefix: &str,
    memory_ids: &[String],
    merged_count: usize,
    duplicate_count: usize,
) -> Result<Vec<memory_core::FoundryJobSpec>, String> {
    if memory_ids.is_empty() {
        return Ok(Vec::new());
    }

    let specs = vec![
        build_foundry_maintenance_job(
            server,
            memory_core::FoundryJobKind::MemoryRerank,
            agent_id,
            path_prefix,
            memory_ids,
            json!({
                "kind": "memory_rerank",
                "neighbor_limit": FOUNDRY_RELATED_LIMIT,
                "merged_count": merged_count,
                "duplicate_count": duplicate_count,
            }),
        ),
        build_foundry_maintenance_job(
            server,
            memory_core::FoundryJobKind::MemoryDistill,
            agent_id,
            path_prefix,
            memory_ids,
            json!({
                "kind": "memory_distill",
                "window": FOUNDRY_DISTILL_WINDOW,
            }),
        ),
        build_foundry_maintenance_job(
            server,
            memory_core::FoundryJobKind::ForgetSweep,
            agent_id,
            path_prefix,
            memory_ids,
            json!({
                "kind": "forget_sweep",
                "keep_latest": FOUNDRY_DISTILL_KEEP,
            }),
        ),
    ];

    for spec in &specs {
        server.enqueue_foundry_job(FoundryMaintenanceItem {
            job: spec.clone(),
            target_db,
            named_project: named_project.clone(),
            path_prefix: path_prefix.to_string(),
            memory_ids: memory_ids.to_vec(),
        })?;
    }

    Ok(specs)
}

pub(crate) fn enqueue_foundry_capture_maintenance(
    server: &MemoryServer,
    target_db: DbScope,
    named_project: Option<String>,
    agent_id: &str,
    path_prefix: &str,
    memory_ids: &[String],
) -> Result<Vec<memory_core::FoundryJobSpec>, String> {
    enqueue_capture_maintenance_jobs(
        server,
        target_db,
        named_project,
        agent_id,
        path_prefix,
        memory_ids,
        0,
        0,
    )
}

pub(super) fn with_foundry_store<T>(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
    f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
) -> Result<T, String> {
    if let Some(project_name) = item.named_project.as_deref() {
        server.with_named_project_store(project_name, f)
    } else {
        server.with_store_for_scope(item.target_db, f)
    }
}

pub(super) fn with_foundry_store_read<T>(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
    f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
) -> Result<T, String> {
    if let Some(project_name) = item.named_project.as_deref() {
        server.with_named_project_store_read(project_name, f)
    } else {
        server.with_store_for_scope_read(item.target_db, f)
    }
}

pub(super) fn memory_claim_signature(entry: &MemoryEntry) -> String {
    format!(
        "{}:r{}:vec{}:arch{}",
        entry.id,
        entry.revision,
        if entry.vector.is_some() { 1 } else { 0 },
        if entry.archived { 1 } else { 0 }
    )
}

pub(super) fn build_foundry_event_hash(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
) -> Result<String, String> {
    let mut signatures = Vec::with_capacity(item.memory_ids.len());
    for memory_id in &item.memory_ids {
        let maybe_entry = with_foundry_store_read(server, item, |store| {
            store
                .get(memory_id)
                .map_err(|e| format!("Failed to load memory {} for claim hash: {e}", memory_id))
        })?;
        match maybe_entry {
            Some(entry) => signatures.push(memory_claim_signature(&entry)),
            None => signatures.push(format!("{memory_id}:missing")),
        }
    }
    signatures.sort();

    Ok(stable_hash(&format!(
        "{}:{}:{}:{}",
        foundry_job_label(&item.job.kind),
        item.named_project.as_deref().unwrap_or("default"),
        item.path_prefix,
        signatures.join(","),
    )))
}

pub(super) fn merge_foundry_metadata(
    existing: &serde_json::Value,
    patch: serde_json::Value,
) -> serde_json::Value {
    let mut root = match existing {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut foundry = root
        .get("foundry")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(patch_obj) = patch.as_object() {
        for (key, value) in patch_obj {
            foundry.insert(key.clone(), value.clone());
        }
    }
    root.insert("foundry".into(), serde_json::Value::Object(foundry));
    serde_json::Value::Object(root)
}

pub(super) fn update_entry_metadata(
    store: &mut MemoryStore,
    entry: &MemoryEntry,
    metadata: &serde_json::Value,
) -> Result<bool, String> {
    store
        .update_with_revision(
            &entry.id,
            &entry.text,
            &entry.summary,
            &entry.source,
            metadata,
            entry.vector.as_deref(),
            entry.revision,
        )
        .map_err(|e| format!("Failed to update foundry metadata for {}: {e}", entry.id))
}

pub(super) fn build_foundry_distill_root(agent_id: &str) -> String {
    format!("{}/distilled", build_foundry_agent_root(agent_id))
}

async fn process_memory_rerank_job(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
) -> Result<usize, String> {
    let mut updated = 0usize;

    for memory_id in &item.memory_ids {
        let Some(entry) = with_foundry_store_read(server, item, |store| {
            store
                .get(memory_id)
                .map_err(|e| format!("Failed to load memory {} for rerank: {e}", memory_id))
        })?
        else {
            continue;
        };

        let Some(vector) = entry.vector.clone() else {
            continue;
        };

        let neighbors = search_similar_capture_entries(
            server,
            item.target_db,
            item.named_project.as_deref(),
            &item.path_prefix,
            &vector,
            FOUNDRY_RELATED_LIMIT + 2,
        )?;

        let mut best_neighbor: Option<memory_core::SearchResult> = None;
        let related = neighbors
            .into_iter()
            .filter(|row| row.entry.id != entry.id)
            .inspect(|row| {
                if best_neighbor.is_none() {
                    best_neighbor = Some(row.clone());
                }
            })
            .take(FOUNDRY_RELATED_LIMIT)
            .map(|row| {
                json!({
                    "id": row.entry.id,
                    "topic": row.entry.topic,
                    "path": row.entry.path,
                    "score": round3(row.score.vector),
                })
            })
            .collect::<Vec<_>>();

        if let Some(similar) = best_neighbor {
            let similarity = similar.score.vector;
            if similarity >= CAPTURE_DEDUP_THRESHOLD {
                let changed = with_foundry_store(server, item, |store| {
                    store.archive_memory(&entry.id).map_err(|e| {
                        format!("Failed to archive duplicate memory {}: {e}", entry.id)
                    })
                })?;
                if changed {
                    updated += 1;
                }
                continue;
            }

            if similarity >= CAPTURE_MERGE_THRESHOLD {
                let merged = merge_capture_entries(&similar.entry, &entry, similarity);
                persist_capture_entry(
                    server,
                    item.target_db,
                    item.named_project.as_deref(),
                    &merged,
                )?;
                let changed = with_foundry_store(server, item, |store| {
                    store
                        .archive_memory(&entry.id)
                        .map_err(|e| format!("Failed to archive merged memory {}: {e}", entry.id))
                })?;
                if changed {
                    updated += 1;
                }
                queue_capture_enrichment(
                    server,
                    item.target_db,
                    item.named_project.clone(),
                    &merged,
                    true,
                    item.job.target_agent_id.as_deref(),
                    Some(&item.path_prefix),
                );
                continue;
            }
        }

        if related.is_empty() {
            continue;
        }

        let metadata = merge_foundry_metadata(
            &entry.metadata,
            json!({
                "last_reranked_at": Utc::now().to_rfc3339(),
                "rerank_job_id": item.job.id,
                "related_entries": related,
            }),
        );

        let applied = with_foundry_store(server, item, |store| {
            update_entry_metadata(store, &entry, &metadata)
        })?;
        if applied {
            updated += 1;
        }
    }

    Ok(updated)
}

async fn process_memory_distill_job(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
) -> Result<Option<String>, String> {
    let source_entries = with_foundry_store_read(server, item, |store| {
        store
            .list_by_path(&item.path_prefix, FOUNDRY_DISTILL_WINDOW, false)
            .map_err(|e| format!("Failed to load recent memories for distill: {e}"))
    })?;

    let source_entries = source_entries
        .into_iter()
        .filter(|entry| entry.source != FOUNDRY_DISTILL_SOURCE)
        .collect::<Vec<_>>();
    if source_entries.is_empty() {
        return Ok(None);
    }

    let distill_text = server
        .llm
        .generate_summary(&build_distill_input(&source_entries))
        .await
        .map_err(|e| format!("Foundry distill summary failed: {e}"))?;
    if distill_text.trim().is_empty() {
        return Ok(None);
    }

    let agent_id = item
        .job
        .target_agent_id
        .as_deref()
        .unwrap_or("unknown-agent");
    let distill_root = build_foundry_distill_root(agent_id);
    let timestamp = Utc::now().to_rfc3339();
    let memory_id = uuid::Uuid::new_v4().to_string();
    let metadata = crate::provenance::inject_provenance(
        server,
        json!({
            "source_memory_ids": source_entries.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(),
            "source_path_prefix": item.path_prefix,
            "job_id": item.job.id,
        }),
        "foundry_worker",
        "memory_distill",
        Some(if item.target_db == DbScope::Project {
            "project"
        } else {
            "global"
        }),
        item.target_db,
        json!({
            "agent_id": agent_id,
            "path_prefix": item.path_prefix,
        }),
    );

    let distill_entry = MemoryEntry {
        id: memory_id.clone(),
        path: format!("{distill_root}/{}", Utc::now().format("%Y%m%dT%H%M%S")),
        summary: distill_text.chars().take(100).collect(),
        text: distill_text,
        importance: 0.75,
        timestamp,
        category: "other".to_string(),
        topic: "foundry_distill".to_string(),
        keywords: vec!["foundry".to_string(), "distill".to_string()],
        persons: Vec::new(),
        entities: dedup_strings(
            source_entries
                .iter()
                .flat_map(|entry| entry.entities.clone())
                .collect::<Vec<_>>(),
        ),
        location: item.path_prefix.clone(),
        source: FOUNDRY_DISTILL_SOURCE.to_string(),
        scope: if item.target_db == DbScope::Project {
            "project".to_string()
        } else {
            "global".to_string()
        },
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata,
        vector: None,
        retention_policy: None,
        domain: None,
    };

    with_foundry_store(server, item, |store| {
        store
            .upsert(&distill_entry)
            .map_err(|e| format!("Failed to save foundry distill memory: {e}"))
    })?;
    queue_capture_enrichment(
        server,
        item.target_db,
        item.named_project.clone(),
        &distill_entry,
        false,
        None,
        None,
    );

    Ok(Some(memory_id))
}

fn process_forget_sweep_job(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
) -> Result<usize, String> {
    let agent_id = item
        .job
        .target_agent_id
        .as_deref()
        .unwrap_or("unknown-agent");
    let distill_root = build_foundry_distill_root(agent_id);
    let distill_entries = with_foundry_store_read(server, item, |store| {
        store
            .list_by_path(&distill_root, FOUNDRY_DISTILL_KEEP + 12, false)
            .map_err(|e| format!("Failed to list foundry distill memories: {e}"))
    })?;

    let mut distill_entries = distill_entries
        .into_iter()
        .filter(|entry| entry.source == FOUNDRY_DISTILL_SOURCE)
        .collect::<Vec<_>>();
    distill_entries.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.path.cmp(&a.path))
            .then_with(|| b.id.cmp(&a.id))
    });

    let stale_ids = distill_entries
        .into_iter()
        .skip(FOUNDRY_DISTILL_KEEP)
        .map(|entry| entry.id)
        .collect::<Vec<_>>();

    let mut archived = 0usize;
    for stale_id in stale_ids {
        let changed = with_foundry_store(server, item, |store| {
            store
                .archive_memory(&stale_id)
                .map_err(|e| format!("Failed to archive stale foundry distill {}: {e}", stale_id))
        })?;
        if changed {
            archived += 1;
        }
    }

    Ok(archived)
}

async fn handle_foundry_maintenance_item(
    server: &MemoryServer,
    item: &FoundryMaintenanceItem,
) -> Result<memory_core::FoundryJobStatus, String> {
    let worker = foundry_worker_name(&item.job.kind);
    let event_hash = build_foundry_event_hash(server, item)?;
    let claimed = with_foundry_store(server, item, |store| {
        store
            .try_claim_event(&event_hash, &item.job.id, worker)
            .map_err(|e| format!("Failed to claim foundry job {}: {e}", item.job.id))
    })?;

    if !claimed {
        return Ok(memory_core::FoundryJobStatus::Skipped);
    }

    let result = match item.job.kind {
        memory_core::FoundryJobKind::MemoryRerank => process_memory_rerank_job(server, item)
            .await
            .map(|_| memory_core::FoundryJobStatus::Completed),
        memory_core::FoundryJobKind::MemoryDistill => process_memory_distill_job(server, item)
            .await
            .map(|_| memory_core::FoundryJobStatus::Completed),
        memory_core::FoundryJobKind::ForgetSweep => {
            process_forget_sweep_job(server, item).map(|_| memory_core::FoundryJobStatus::Completed)
        }
        _ => Ok(memory_core::FoundryJobStatus::Skipped),
    };

    if let Err(err) = &result {
        let _ = with_foundry_store(server, item, |store| {
            store
                .release_event_claim(&event_hash, worker)
                .map_err(|e| format!("Failed to release foundry job claim {}: {e}", item.job.id))
        });
        return Err(err.clone());
    }

    result
}

pub(crate) async fn run_foundry_maintenance_worker(
    server: MemoryServer,
    mut rx: mpsc::Receiver<FoundryMaintenanceItem>,
) {
    while let Some(item) = rx.recv().await {
        server.foundry_stats.queued.fetch_sub(1, Ordering::Relaxed);
        server.foundry_stats.running.fetch_add(1, Ordering::Relaxed);

        let result = handle_foundry_maintenance_item(&server, &item).await;

        server.foundry_stats.running.fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(memory_core::FoundryJobStatus::Skipped) => {
                server.foundry_stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {
                server
                    .foundry_stats
                    .completed
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                eprintln!("[foundry-worker] job {} failed: {err}", item.job.id);
                server.foundry_stats.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    eprintln!("[foundry-worker] channel closed, worker exiting");
}

fn build_distill_input(entries: &[MemoryEntry]) -> String {
    entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let summary = if entry.summary.trim().is_empty() {
                entry.text.chars().take(180).collect::<String>()
            } else {
                entry.summary.clone()
            };
            format!(
                "Memory {} | topic={} | importance={:.2}\nSummary: {}\nText: {}",
                idx + 1,
                if entry.topic.is_empty() {
                    "unknown"
                } else {
                    &entry.topic
                },
                entry.importance,
                summary,
                entry.text.chars().take(320).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
