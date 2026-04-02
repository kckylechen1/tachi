use super::*;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};

const CAPTURE_DEDUP_THRESHOLD: f64 = 0.95;
const CAPTURE_MERGE_THRESHOLD: f64 = 0.85;
const FOUNDRY_DISTILL_SOURCE: &str = "foundry_distill";
const FOUNDRY_RELATED_LIMIT: usize = 4;
const FOUNDRY_DISTILL_WINDOW: usize = 8;
const FOUNDRY_DISTILL_KEEP: usize = 6;

#[derive(Debug, Default)]
pub(super) struct FoundryWorkerStats {
    pub queued: AtomicU64,
    pub running: AtomicU64,
    pub completed: AtomicU64,
    pub failed: AtomicU64,
    pub skipped: AtomicU64,
}

#[derive(Debug, Clone)]
pub(super) struct FoundryMaintenanceItem {
    pub job: memory_core::FoundryJobSpec,
    pub target_db: DbScope,
    pub named_project: Option<String>,
    pub path_prefix: String,
    pub memory_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionCaptureDraft {
    text: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    topic: String,
    #[serde(default = "default_capture_category")]
    category: String,
    #[serde(default = "default_capture_scope")]
    scope: String,
    #[serde(default = "default_capture_importance")]
    importance: f64,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    persons: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    location: String,
}

fn default_capture_category() -> String {
    "fact".to_string()
}

fn default_capture_scope() -> String {
    "project".to_string()
}

fn default_capture_importance() -> f64 {
    0.7
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn normalize_scope(raw: &str, fallback: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" => "user".to_string(),
        "project" => "project".to_string(),
        "general" => "general".to_string(),
        _ => fallback.to_string(),
    }
}

fn normalize_category(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "fact" => "fact".to_string(),
        "decision" => "decision".to_string(),
        "preference" => "preference".to_string(),
        "entity" => "entity".to_string(),
        "experience" => "experience".to_string(),
        _ => "other".to_string(),
    }
}

fn dedup_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn build_entry_path(base_path: &str, topic: &str) -> String {
    let base = base_path.trim_end_matches('/');
    let topic_segment = sanitize_safe_path_name(topic.trim());
    if topic_segment.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{topic_segment}")
    }
}

fn normalize_path_prefix_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "/" {
        return Some("/".to_string());
    }
    Some(trimmed.trim_end_matches('/').to_string())
}

fn path_is_within_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn build_openclaw_agent_root(agent_id: &str) -> String {
    format!("/openclaw/agent-{}", sanitize_safe_path_name(agent_id))
}

fn build_foundry_agent_root(agent_id: &str) -> String {
    format!("/foundry/agents/{}", sanitize_safe_path_name(agent_id))
}

fn merge_text(existing: &str, incoming: &str) -> String {
    let existing = existing.trim();
    let incoming = incoming.trim();
    if existing.is_empty() {
        return incoming.to_string();
    }
    if incoming.is_empty() || existing == incoming || existing.contains(incoming) {
        return existing.to_string();
    }
    if incoming.contains(existing) {
        return incoming.to_string();
    }
    format!("{existing}\n{incoming}")
}

fn merge_summary(existing: &str, incoming: &str, merged_text: &str) -> String {
    let existing = existing.trim();
    let incoming = incoming.trim();
    if existing.is_empty() && incoming.is_empty() {
        return merged_text.chars().take(100).collect();
    }
    if existing.is_empty() {
        return incoming.to_string();
    }
    if incoming.is_empty() || existing == incoming || existing.contains(incoming) {
        return existing.to_string();
    }
    if incoming.contains(existing) {
        return incoming.to_string();
    }
    format!("{existing}; {incoming}").chars().take(100).collect()
}

fn merge_category(existing: &str, incoming: &str) -> String {
    for preferred in ["decision", "preference", "entity", "experience", "fact"] {
        if existing == preferred || incoming == preferred {
            return preferred.to_string();
        }
    }
    "other".to_string()
}

fn merge_metadata(
    existing: &serde_json::Value,
    incoming: &serde_json::Value,
    incoming_id: &str,
    similarity: f64,
) -> serde_json::Value {
    let mut merged = match existing {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };

    let mut source_refs = Vec::<serde_json::Value>::new();
    for metadata in [existing, incoming] {
        if let Some(items) = metadata.get("source_refs").and_then(|value| value.as_array()) {
            for item in items {
                if !source_refs.contains(item) {
                    source_refs.push(item.clone());
                }
            }
        }
    }
    if !source_refs.is_empty() {
        merged.insert("source_refs".into(), serde_json::Value::Array(source_refs));
    }

    let mut merge_history = merged
        .get("merge_history")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    merge_history.push(json!({
        "merged_at": Utc::now().to_rfc3339(),
        "incoming_id": incoming_id,
        "similarity": round3(similarity),
        "strategy": "inline_foundry_merge",
    }));
    merged.insert(
        "merge_history".into(),
        serde_json::Value::Array(merge_history),
    );

    serde_json::Value::Object(merged)
}

fn merge_capture_entries(
    existing: &MemoryEntry,
    incoming: &MemoryEntry,
    similarity: f64,
) -> MemoryEntry {
    let merged_text = merge_text(&existing.text, &incoming.text);
    let merged_summary = merge_summary(&existing.summary, &incoming.summary, &merged_text);
    let timestamp = if incoming.timestamp > existing.timestamp {
        incoming.timestamp.clone()
    } else {
        existing.timestamp.clone()
    };

    MemoryEntry {
        id: existing.id.clone(),
        path: if existing.path.trim().is_empty() {
            incoming.path.clone()
        } else {
            existing.path.clone()
        },
        summary: merged_summary,
        text: merged_text,
        importance: existing.importance.max(incoming.importance),
        timestamp,
        category: merge_category(&existing.category, &incoming.category),
        topic: if existing.topic.trim().is_empty() {
            incoming.topic.clone()
        } else {
            existing.topic.clone()
        },
        keywords: dedup_strings(
            existing
                .keywords
                .iter()
                .cloned()
                .chain(incoming.keywords.iter().cloned())
                .collect(),
        ),
        persons: dedup_strings(
            existing
                .persons
                .iter()
                .cloned()
                .chain(incoming.persons.iter().cloned())
                .collect(),
        ),
        entities: dedup_strings(
            existing
                .entities
                .iter()
                .cloned()
                .chain(incoming.entities.iter().cloned())
                .collect(),
        ),
        location: if incoming.location.trim().is_empty() {
            existing.location.clone()
        } else {
            incoming.location.clone()
        },
        source: "capture_session".to_string(),
        scope: if incoming.scope == "general" {
            existing.scope.clone()
        } else {
            incoming.scope.clone()
        },
        archived: false,
        access_count: existing.access_count,
        last_access: existing.last_access.clone(),
        revision: existing.revision + 1,
        metadata: merge_metadata(&existing.metadata, &incoming.metadata, &incoming.id, similarity),
        vector: None,
    }
}

fn capture_search_options(query_vec: Vec<f32>, path_prefix: Option<String>, vec_available: bool) -> SearchOptions {
    SearchOptions {
        top_k: 1,
        path_prefix,
        query_vec: Some(query_vec),
        include_archived: false,
        candidates_per_channel: 8,
        mmr_threshold: None,
        graph_expand_hops: 0,
        graph_relation_filter: None,
        vec_available,
        weights: HybridWeights {
            semantic: 1.0,
            fts: 0.0,
            symbolic: 0.0,
            decay: 0.0,
        },
        record_access: false,
    }
}

fn find_similar_capture_entry(
    server: &MemoryServer,
    target_db: DbScope,
    named_project: Option<&str>,
    path_prefix: &str,
    query_vec: &[f32],
) -> Result<Option<memory_core::SearchResult>, String> {
    if query_vec.is_empty() {
        return Ok(None);
    }

    let search_action = |store: &mut MemoryStore| {
        let results = store
            .search(
                "",
                Some(capture_search_options(
                    query_vec.to_vec(),
                    Some(path_prefix.to_string()),
                    store.vec_available,
                )),
            )
            .map_err(|e| format!("Failed to search similar capture entry: {e}"))?;
        Ok(results.into_iter().next())
    };

    if let Some(project_name) = named_project {
        server.with_named_project_store_read(project_name, search_action)
    } else {
        server.with_store_for_scope_read(target_db, search_action)
    }
}

fn persist_capture_entry(
    server: &MemoryServer,
    target_db: DbScope,
    named_project: Option<&str>,
    entry: &MemoryEntry,
) -> Result<(), String> {
    if let Some(project_name) = named_project {
        server.with_named_project_store(project_name, |store| {
            store
                .upsert(entry)
                .map_err(|e| format!("Failed to save session capture to '{project_name}': {e}"))
        })
    } else {
        server.with_store_for_scope(target_db, |store| {
            store
                .upsert(entry)
                .map_err(|e| format!("Failed to save captured memory: {e}"))
        })
    }
}

fn queue_capture_enrichment(
    server: &MemoryServer,
    target_db: DbScope,
    named_project: Option<String>,
    entry: &MemoryEntry,
    needs_summary: bool,
) {
    let _ = server.enrich_tx.send(EnrichmentItem {
        id: entry.id.clone(),
        text: entry.text.clone(),
        needs_embedding: true,
        needs_summary,
        target_db,
        named_project,
        revision: entry.revision,
    });
}

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

fn foundry_job_label(kind: &memory_core::FoundryJobKind) -> &'static str {
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

fn enqueue_capture_maintenance_jobs(
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

fn with_foundry_store<T>(
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

fn with_foundry_store_read<T>(
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

fn build_foundry_event_hash(item: &FoundryMaintenanceItem) -> String {
    stable_hash(&format!(
        "{}:{}:{}:{}",
        foundry_job_label(&item.job.kind),
        item.named_project.as_deref().unwrap_or("default"),
        item.path_prefix,
        item.memory_ids.join(","),
    ))
}

fn merge_foundry_metadata(
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

fn update_entry_metadata(
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

fn build_foundry_distill_root(agent_id: &str) -> String {
    format!("{}/distilled", build_foundry_agent_root(agent_id))
}

#[derive(Debug, Clone)]
struct RecallScope {
    search_prefixes: Vec<Option<String>>,
    allowed_prefixes: Vec<String>,
    warning: Option<String>,
}

fn resolve_recall_scope(path_prefix: Option<&str>, agent_id: Option<&str>) -> RecallScope {
    let requested = path_prefix.and_then(normalize_path_prefix_value);
    let Some(agent_id) = agent_id else {
        return RecallScope {
            search_prefixes: vec![requested],
            allowed_prefixes: Vec::new(),
            warning: None,
        };
    };

    let openclaw_root = build_openclaw_agent_root(agent_id);
    let foundry_root = build_foundry_agent_root(agent_id);
    let foundry_distill_root = build_foundry_distill_root(agent_id);
    let allowed_prefixes = vec![openclaw_root.clone(), foundry_root.clone()];

    match requested {
        Some(prefix)
            if allowed_prefixes
                .iter()
                .any(|allowed| path_is_within_prefix(&prefix, allowed)) =>
        {
            RecallScope {
                search_prefixes: vec![Some(prefix)],
                allowed_prefixes,
                warning: None,
            }
        }
        Some(prefix) => RecallScope {
            search_prefixes: vec![Some(openclaw_root.clone()), Some(foundry_distill_root)],
            allowed_prefixes,
            warning: Some(format!(
                "path_prefix '{}' was outside agent scope; clamped to {}",
                prefix, openclaw_root
            )),
        },
        None => RecallScope {
            search_prefixes: vec![Some(openclaw_root), Some(foundry_distill_root)],
            allowed_prefixes,
            warning: None,
        },
    }
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
        })? else {
            continue;
        };

        let Some(vector) = entry.vector.clone() else {
            continue;
        };

        let neighbors = with_foundry_store_read(server, item, |store| {
            store
                .search(
                    "",
                    Some(capture_search_options(
                        vector.clone(),
                        Some(item.path_prefix.clone()),
                        store.vec_available,
                    )),
                )
                .map_err(|e| format!("Failed to rerank neighbors for {}: {e}", entry.id))
        })?;

        let related = neighbors
            .into_iter()
            .filter(|row| row.entry.id != entry.id)
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
                if entry.topic.is_empty() { "unknown" } else { &entry.topic },
                entry.importance,
                summary,
                entry.text.chars().take(320).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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
        Some(if item.target_db == DbScope::Project { "project" } else { "global" }),
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

    let stale_ids = distill_entries
        .into_iter()
        .filter(|entry| entry.source == FOUNDRY_DISTILL_SOURCE)
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
    let event_hash = build_foundry_event_hash(item);
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
            process_forget_sweep_job(server, item)
                .map(|_| memory_core::FoundryJobStatus::Completed)
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

pub(super) async fn run_foundry_maintenance_worker(
    server: MemoryServer,
    mut rx: mpsc::UnboundedReceiver<FoundryMaintenanceItem>,
) {
    while let Some(item) = rx.recv().await {
        server
            .foundry_stats
            .queued
            .fetch_sub(1, Ordering::Relaxed);
        server
            .foundry_stats
            .running
            .fetch_add(1, Ordering::Relaxed);

        let result = handle_foundry_maintenance_item(&server, &item).await;

        server
            .foundry_stats
            .running
            .fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(memory_core::FoundryJobStatus::Skipped) => {
                server
                    .foundry_stats
                    .skipped
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {
                server
                    .foundry_stats
                    .completed
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                eprintln!("[foundry-worker] job {} failed: {err}", item.job.id);
                server
                    .foundry_stats
                    .failed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    eprintln!("[foundry-worker] channel closed, worker exiting");
}

fn value_text(row: &Value) -> String {
    row.get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn value_summary(row: &Value) -> String {
    row.get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn value_id(row: &Value) -> String {
    row.get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn value_path(row: &Value) -> String {
    row.get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn value_topic(row: &Value) -> String {
    row.get("topic")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn value_relevance(row: &Value) -> f64 {
    row.get("relevance")
        .and_then(Value::as_f64)
        .or_else(|| {
            row.get("score")
                .and_then(Value::as_object)
                .and_then(|score| score.get("final"))
                .and_then(Value::as_f64)
        })
        .unwrap_or(0.0)
}

fn value_string_array(row: &Value, key: &str) -> Vec<String> {
    row.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn build_rerank_document(row: &Value) -> String {
    let text = value_text(row);
    let topic = value_topic(row);
    let keywords = value_string_array(row, "keywords");
    [text, topic, keywords.join(", ")]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_prepend_context(rows: &[Value]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let memory_lines = rows
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let id = row.get("id").and_then(Value::as_str).unwrap_or("unknown");
            let topic = value_topic(row);
            let relevance = value_relevance(row);
            let summary = value_summary(row);
            let text = value_text(row);
            let keywords = value_string_array(row, "keywords").join(", ");
            let persons = value_string_array(row, "persons").join(", ");

            [
                format!(
                    "M-ENTRY #{} [ID={}] [Topic={}] [Score={:.2}]",
                    idx + 1,
                    id,
                    if topic.is_empty() { "unknown" } else { &topic },
                    relevance
                ),
                format!(
                    "Summary: {}",
                    if summary.is_empty() {
                        text.chars().take(80).collect::<String>()
                    } else {
                        summary
                    }
                ),
                format!("Keywords: {} | Persons: {}", keywords, persons),
            ]
            .join("\n")
        })
        .collect::<Vec<_>>();

    let mut entity_links = Vec::new();
    for row in rows {
        let entities = value_string_array(row, "entities");
        if entities.len() >= 2 {
            entity_links.push(entities.join(" ↔ "));
        }
    }

    let mut block = format!(
        "\n<relevant-structured-memories>\n{}\n",
        memory_lines.join("\n\n")
    );
    if !entity_links.is_empty() {
        let deduped = dedup_strings(entity_links);
        block.push_str(&format!("\nEntity connections: {}\n", deduped.join(", ")));
    }
    block.push_str("</relevant-structured-memories>\n");
    block
}

fn parse_session_capture_response(raw: &str) -> Result<Vec<SessionCaptureDraft>, String> {
    let json_str = llm::LlmClient::strip_code_fence(raw);
    let parsed: Vec<SessionCaptureDraft> = serde_json::from_str(json_str).map_err(|e| {
        format!(
            "Failed to parse session capture JSON: {e} — response was: {}",
            json_str
        )
    })?;

    Ok(parsed
        .into_iter()
        .filter(|draft| !draft.text.trim().is_empty())
        .collect())
}

async fn rerank_rows(
    server: &MemoryServer,
    query: &str,
    rows: Vec<Value>,
    top_k: usize,
) -> Vec<Value> {
    if rows.len() <= 1 {
        return rows.into_iter().take(top_k).collect();
    }

    let docs = rows.iter().map(build_rerank_document).collect::<Vec<_>>();
    match server.llm.rerank_voyage(query, &docs, top_k).await {
        Ok(order) => {
            let mut out = Vec::with_capacity(order.len());
            for (index, score) in order {
                let Some(mut row) = rows.get(index).cloned() else {
                    continue;
                };
                if let Value::Object(map) = &mut row {
                    map.insert("relevance".into(), json!(round3(score)));
                    map.insert("rerank_score".into(), json!(round3(score)));
                    if let Some(Value::Object(score_map)) = map.get_mut("score") {
                        score_map.insert("final".into(), json!(round3(score)));
                    }
                }
                out.push(row);
            }
            if out.is_empty() {
                rows.into_iter().take(top_k).collect()
            } else {
                out
            }
        }
        Err(err) => {
            eprintln!("[recall_context] rerank failed, falling back to hybrid ranking: {err}");
            rows.into_iter().take(top_k).collect()
        }
    }
}

pub(super) async fn handle_recall_context(
    server: &MemoryServer,
    params: RecallContextParams,
) -> Result<String, String> {
    if params.query.trim().is_empty() {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "empty_query",
            "count": 0,
            "results": [],
            "prepend_context": "",
        }))
        .map_err(|e| format!("Failed to serialize recall_context response: {e}"));
    }

    let candidate_multiplier = params.candidate_multiplier.max(1);
    let candidate_top_k = params.top_k.max(1).saturating_mul(candidate_multiplier);
    let recall_scope = resolve_recall_scope(
        params.path_prefix.as_deref(),
        params.agent_id.as_deref(),
    );
    let mut parsed = Vec::<Value>::new();
    let mut seen_ids = HashSet::<String>::new();
    for search_prefix in &recall_scope.search_prefixes {
        let raw_results = handle_search_memory(
            server,
            SearchMemoryParams {
                query: params.query.clone(),
                query_vec: None,
                top_k: candidate_top_k,
                path_prefix: search_prefix.clone(),
                include_archived: false,
                candidates_per_channel: candidate_top_k.max(20),
                mmr_threshold: None,
                graph_expand_hops: 0,
                graph_relation_filter: None,
                weights: None,
                agent_role: params.agent_role.clone(),
                project: params.project.clone(),
            },
        )
        .await?;

        let rows: Vec<Value> = serde_json::from_str(&raw_results).map_err(|e| {
            format!("Failed to parse search_memory results for recall_context: {e}")
        })?;
        for row in rows {
            let id = value_id(&row);
            if !id.is_empty() && !seen_ids.insert(id) {
                continue;
            }
            parsed.push(row);
        }
    }

    let excluded_topics = params
        .exclude_topics
        .iter()
        .map(|topic| topic.trim().to_ascii_lowercase())
        .filter(|topic| !topic.is_empty())
        .collect::<HashSet<_>>();

    let min_score = params.min_score.unwrap_or(0.0);

    let filtered = parsed
        .into_iter()
        .filter(|row| {
            if recall_scope.allowed_prefixes.is_empty() {
                return true;
            }
            let path = value_path(row);
            !path.is_empty()
                && recall_scope
                    .allowed_prefixes
                    .iter()
                    .any(|prefix| path_is_within_prefix(&path, prefix))
        })
        .filter(|row| row.get("l0_rule").and_then(Value::as_bool) != Some(true))
        .filter(|row| {
            let topic = value_topic(row);
            topic.is_empty() || !excluded_topics.contains(&topic.to_ascii_lowercase())
        })
        .filter(|row| value_relevance(row) >= min_score)
        .collect::<Vec<_>>();

    let reranked = rerank_rows(server, &params.query, filtered, params.top_k.max(1)).await;
    let final_rows = reranked
        .into_iter()
        .filter(|row| value_relevance(row) >= min_score)
        .collect::<Vec<_>>();
    let prepend_context = build_prepend_context(&final_rows);

    serde_json::to_string(&json!({
        "status": "completed",
        "count": final_rows.len(),
        "results": final_rows,
        "prepend_context": prepend_context,
        "path_prefixes": recall_scope.search_prefixes,
        "allowed_prefixes": recall_scope.allowed_prefixes,
        "warning": recall_scope.warning,
    }))
    .map_err(|e| format!("Failed to serialize recall_context response: {e}"))
}

pub(super) async fn handle_capture_session(
    server: &MemoryServer,
    params: CaptureSessionParams,
) -> Result<String, String> {
    let combined_text = params
        .messages
        .iter()
        .map(|message| format!("{}: {}", message.role.trim(), message.content.trim()))
        .collect::<Vec<_>>()
        .join("\n");

    if combined_text.trim().is_empty() {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "empty_messages",
            "captured": 0,
        }))
        .map_err(|e| format!("Failed to serialize capture_session response: {e}"));
    }

    if !params.force && combined_text.chars().count() < params.min_chars {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "below_min_chars",
            "captured": 0,
        }))
        .map_err(|e| format!("Failed to serialize capture_session response: {e}"));
    }

    let payload = json!({
        "conversation_id": params.conversation_id,
        "turn_id": params.turn_id,
        "agent_id": params.agent_id,
        "messages": params.messages,
    });
    let request = serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("Failed to serialize session capture payload: {e}"))?;
    let raw = server
        .llm
        .call_llm(
            crate::prompts::SESSION_CAPTURE_PROMPT,
            &request,
            None,
            0.1,
            2400,
        )
        .await?;
    let drafts = parse_session_capture_response(&raw)?;

    if drafts.is_empty() {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "no_durable_memories",
            "captured": 0,
        }))
        .map_err(|e| format!("Failed to serialize capture_session response: {e}"));
    }

    let texts = drafts
        .iter()
        .map(|draft| draft.text.clone())
        .collect::<Vec<_>>();
    let embeddings = match server.llm.embed_voyage_batch(&texts, "document").await {
        Ok(vectors) => Some(vectors),
        Err(err) => {
            eprintln!("[capture_session] embedding failed, deferring enrichment: {err}");
            None
        }
    };

    let requested_scope = normalize_scope(&params.scope, "project");
    let named_project = params.project.clone();
    let (target_db, warning) = if named_project.is_some() {
        (DbScope::Project, None)
    } else {
        server.resolve_write_scope(&requested_scope)
    };

    let base_path = params.path_prefix.clone().unwrap_or_else(|| {
        format!(
            "/openclaw/agent-{}",
            sanitize_safe_path_name(&params.agent_id)
        )
    });
    let source_ref_id = format!("{}:{}", params.conversation_id, params.turn_id);

    let entries = drafts
        .into_iter()
        .enumerate()
        .map(|(idx, draft)| {
            let topic = if draft.topic.trim().is_empty() {
                "session_capture".to_string()
            } else {
                draft.topic.trim().to_string()
            };
            let scope = normalize_scope(&draft.scope, &requested_scope);
            let metadata = crate::provenance::inject_provenance(
                server,
                json!({
                    "source_refs": [{
                        "ref_type": "turn",
                        "ref_id": source_ref_id,
                    }],
                    "conversation_id": params.conversation_id,
                    "turn_id": params.turn_id,
                    "agent_id": params.agent_id,
                    "message_count": params.messages.len(),
                }),
                "capture_session",
                "session_capture",
                Some(scope.as_str()),
                target_db,
                json!({
                    "conversation_id": params.conversation_id,
                    "turn_id": params.turn_id,
                    "agent_id": params.agent_id,
                    "path_prefix": base_path,
                }),
            );

            let summary = if draft.summary.trim().is_empty() {
                draft.text.chars().take(100).collect::<String>()
            } else {
                draft.summary.trim().to_string()
            };

            MemoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                path: build_entry_path(&base_path, &topic),
                summary,
                text: draft.text.trim().to_string(),
                importance: draft.importance.clamp(0.0, 1.0),
                timestamp: Utc::now().to_rfc3339(),
                category: normalize_category(&draft.category),
                topic,
                keywords: dedup_strings(draft.keywords),
                persons: dedup_strings(draft.persons),
                entities: dedup_strings(draft.entities),
                location: draft.location.trim().to_string(),
                source: "capture_session".to_string(),
                scope,
                archived: false,
                access_count: 0,
                last_access: None,
                revision: 1,
                metadata,
                vector: embeddings
                    .as_ref()
                    .and_then(|vectors| vectors.get(idx).cloned()),
            }
        })
        .collect::<Vec<_>>();

    let mut saved_ids = Vec::new();
    let mut merged_ids = Vec::new();
    let mut duplicate_ids = Vec::new();

    for entry in &entries {
        let mut persisted_entry = entry.clone();
        let mut queue_enrichment = embeddings.is_none();
        let mut was_merged = false;

        if let Some(ref vector) = entry.vector {
            if let Some(similar) = find_similar_capture_entry(
                server,
                target_db,
                named_project.as_deref(),
                &base_path,
                vector,
            )? {
                let similarity = similar.score.vector;
                if similarity >= CAPTURE_DEDUP_THRESHOLD {
                    duplicate_ids.push(similar.entry.id);
                    continue;
                }
                if similarity >= CAPTURE_MERGE_THRESHOLD {
                    persisted_entry = merge_capture_entries(&similar.entry, entry, similarity);
                    queue_enrichment = true;
                    was_merged = true;
                    merged_ids.push(persisted_entry.id.clone());
                }
            }
        }

        persist_capture_entry(server, target_db, named_project.as_deref(), &persisted_entry)?;
        if queue_enrichment {
            queue_capture_enrichment(
                server,
                target_db,
                named_project.clone(),
                &persisted_entry,
                was_merged,
            );
        }
        saved_ids.push(persisted_entry.id.clone());
    }

    let saved_ids = dedup_strings(saved_ids);
    let merged_ids = dedup_strings(merged_ids);
    let duplicate_ids = dedup_strings(duplicate_ids);
    let maintenance_jobs = enqueue_capture_maintenance_jobs(
        server,
        target_db,
        named_project.clone(),
        &params.agent_id,
        &base_path,
        &saved_ids,
        merged_ids.len(),
        duplicate_ids.len(),
    )?;

    let mut response = serde_json::Map::new();
    response.insert("status".into(), json!("completed"));
    response.insert("captured".into(), json!(saved_ids.len()));
    response.insert("ids".into(), json!(saved_ids));
    response.insert("merged_ids".into(), json!(merged_ids));
    response.insert("duplicate_ids".into(), json!(duplicate_ids));
    response.insert("duplicates_skipped".into(), json!(duplicate_ids.len()));
    response.insert("maintenance_jobs".into(), json!(maintenance_jobs));
    response.insert("db".into(), json!(target_db.as_str()));
    response.insert("path_prefix".into(), json!(base_path));
    if let Some(warning) = warning {
        response.insert("warning".into(), json!(warning));
    }

    serde_json::to_string(&Value::Object(response))
        .map_err(|e| format!("Failed to serialize capture_session response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_capture_response_accepts_json_array() {
        let parsed = parse_session_capture_response(
            r#"[
              {
                "text":"用户偏好用 Tachi 统一承载记忆能力",
                "summary":"偏好统一记忆本体",
                "topic":"memory_architecture",
                "category":"decision",
                "scope":"project",
                "importance":0.9,
                "keywords":["tachi","memory"],
                "persons":["Kyle"],
                "entities":["OpenClaw"],
                "location":"Sigil"
              }
            ]"#,
        )
        .expect("response should parse");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].category, "decision");
    }

    #[test]
    fn build_prepend_context_formats_memory_block() {
        let block = build_prepend_context(&[json!({
            "id": "m_1",
            "topic": "memory_architecture",
            "summary": "偏好统一记忆本体",
            "text": "用户偏好用 Tachi 统一承载记忆能力",
            "keywords": ["tachi", "memory"],
            "persons": ["Kyle"],
            "entities": ["Tachi", "OpenClaw"],
            "relevance": 0.912,
        })]);

        assert!(block.contains("<relevant-structured-memories>"));
        assert!(block.contains("memory_architecture"));
        assert!(block.contains("Entity connections"));
    }

    #[test]
    fn resolve_recall_scope_defaults_to_agent_memory_and_distill_paths() {
        let scope = resolve_recall_scope(None, Some("main"));

        assert_eq!(
            scope.search_prefixes,
            vec![
                Some("/openclaw/agent-main".to_string()),
                Some("/foundry/agents/main/distilled".to_string()),
            ]
        );
        assert_eq!(
            scope.allowed_prefixes,
            vec![
                "/openclaw/agent-main".to_string(),
                "/foundry/agents/main".to_string(),
            ]
        );
        assert!(scope.warning.is_none());
    }

    #[test]
    fn resolve_recall_scope_clamps_cross_agent_prefix() {
        let scope = resolve_recall_scope(Some("/openclaw/agent-other"), Some("main"));

        assert_eq!(
            scope.search_prefixes,
            vec![
                Some("/openclaw/agent-main".to_string()),
                Some("/foundry/agents/main/distilled".to_string()),
            ]
        );
        assert!(scope
            .warning
            .as_deref()
            .unwrap_or("")
            .contains("outside agent scope"));
    }

    #[test]
    fn merge_capture_entries_unions_core_fields() {
        let existing = MemoryEntry {
            id: "m_existing".to_string(),
            path: "/openclaw/agent-main/memory_architecture".to_string(),
            summary: "偏好统一记忆本体".to_string(),
            text: "用户偏好用 Tachi 统一承载记忆能力".to_string(),
            importance: 0.7,
            timestamp: "2026-04-02T00:00:00Z".to_string(),
            category: "fact".to_string(),
            topic: "memory_architecture".to_string(),
            keywords: vec!["tachi".to_string()],
            persons: vec!["Kyle".to_string()],
            entities: vec!["Tachi".to_string()],
            location: "Sigil".to_string(),
            source: "capture_session".to_string(),
            scope: "project".to_string(),
            archived: false,
            access_count: 1,
            last_access: None,
            revision: 2,
            metadata: json!({
                "source_refs": [{"ref_type":"turn","ref_id":"conv-1:turn-1"}]
            }),
            vector: Some(vec![0.1, 0.2]),
        };

        let incoming = MemoryEntry {
            id: "m_incoming".to_string(),
            path: "/openclaw/agent-main/memory_architecture".to_string(),
            summary: "记忆与 skill 统一优化".to_string(),
            text: "用户希望把记忆和 skill 优化统一收到 Tachi".to_string(),
            importance: 0.9,
            timestamp: "2026-04-02T01:00:00Z".to_string(),
            category: "decision".to_string(),
            topic: "memory_architecture".to_string(),
            keywords: vec!["skill".to_string()],
            persons: vec![],
            entities: vec!["OpenClaw".to_string()],
            location: "".to_string(),
            source: "capture_session".to_string(),
            scope: "project".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            metadata: json!({
                "source_refs": [{"ref_type":"turn","ref_id":"conv-1:turn-2"}]
            }),
            vector: Some(vec![0.3, 0.4]),
        };

        let merged = merge_capture_entries(&existing, &incoming, 0.9);
        assert_eq!(merged.id, "m_existing");
        assert_eq!(merged.category, "decision");
        assert!(merged.text.contains("统一承载记忆能力"));
        assert!(merged.text.contains("skill 优化"));
        assert!(merged.keywords.contains(&"tachi".to_string()));
        assert!(merged.keywords.contains(&"skill".to_string()));
        assert!(merged.entities.contains(&"Tachi".to_string()));
        assert!(merged.entities.contains(&"OpenClaw".to_string()));
        assert!(merged.vector.is_none());
        assert_eq!(merged.revision, 3);
    }
}
