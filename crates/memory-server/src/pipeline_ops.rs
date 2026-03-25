use super::*;

pub(super) async fn handle_extract_facts(
    server: &MemoryServer,
    params: ExtractFactsParams,
) -> Result<String, String> {
    let (target_db, _warning) = server.resolve_write_scope("project");
    let source = params.source.clone();

    let facts = server
        .llm
        .extract_facts(&params.text)
        .await
        .map_err(|e| format!("LLM extraction failed: {e}"))?;

    if facts.is_empty() {
        return Ok(serde_json::to_string(&serde_json::json!({
            "status": "completed",
            "source": source,
            "facts_extracted": 0,
            "facts_saved": 0
        }))
        .unwrap());
    }

    let count = facts.len();
    let saved = server
        .with_store_for_scope(target_db, |store| {
            let mut saved = 0;
            for fact in &facts {
                let Some(entry) =
                    fact_to_entry(fact, "extraction", serde_json::json!({"source": source}))
                else {
                    continue;
                };
                if store.upsert(&entry).is_ok() {
                    saved += 1;
                }
            }
            Ok(saved)
        })
        .map_err(|e| format!("DB write failed: {e}"))?;

    Ok(serde_json::to_string(&serde_json::json!({
        "status": "completed",
        "source": source,
        "facts_extracted": count,
        "facts_saved": saved
    }))
    .unwrap())
}

pub(super) async fn handle_ingest_event(
    server: &MemoryServer,
    params: IngestEventParams,
) -> Result<String, String> {
    let event_hash = stable_hash(&format!("{}:{}", params.conversation_id, params.turn_id));
    let (target_db, _warning) = server.resolve_write_scope("project");

    let combined_text: String = params
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

    if combined_text.trim().is_empty() {
        return Ok(serde_json::to_string(&serde_json::json!({
            "status": "skipped",
            "reason": "No content to process"
        }))
        .unwrap());
    }

    let claimed = server.with_store_for_scope(target_db, |store| {
        store
            .try_claim_event(
                &event_hash,
                &format!("{}:{}", params.conversation_id, params.turn_id),
                "ingest",
            )
            .map_err(|e| format!("Failed to claim event: {e}"))
    })?;
    if !claimed {
        return Ok(serde_json::to_string(&serde_json::json!({
            "status": "skipped",
            "reason": "Event already processed",
            "hash": event_hash
        }))
        .unwrap());
    }

    let server = server.clone();
    let conversation_id = params.conversation_id.clone();
    let turn_id = params.turn_id.clone();
    let eh = event_hash.clone();
    tokio::spawn(async move {
        match server.llm.extract_facts(&combined_text).await {
            Ok(facts) if facts.is_empty() => {
                eprintln!("[ingest_event] no facts extracted from {conversation_id}:{turn_id}");
            }
            Ok(facts) => {
                let count = facts.len();
                let saved = server.with_store_for_scope(target_db, |store| {
                    let mut saved = 0;
                    for fact in &facts {
                        let Some(entry) = fact_to_entry(
                            fact,
                            &format!("conversation:{conversation_id}"),
                            serde_json::json!({
                                "conversation_id": conversation_id,
                                "turn_id": turn_id,
                            }),
                        ) else {
                            continue;
                        };
                        if store.upsert(&entry).is_ok() {
                            saved += 1;
                        }
                    }
                    Ok(saved)
                });
                match saved {
                    Ok(n) => {
                        eprintln!("[ingest_event] saved {n}/{count} facts for {conversation_id}:{turn_id}")
                    }
                    Err(e) => {
                        eprintln!(
                            "[ingest_event] DB write failed: {e} — releasing claim for retry"
                        );
                        let _ = server.with_store_for_scope(target_db, |store| {
                            store
                                .release_event_claim(&eh, "ingest")
                                .map_err(|e| format!("{e}"))
                        });
                        let dl = DeadLetter {
                            id: uuid::Uuid::new_v4().to_string(),
                            tool_name: "ingest_event".to_string(),
                            arguments: Some(serde_json::Map::from_iter([
                                (
                                    "conversation_id".to_string(),
                                    serde_json::json!(conversation_id),
                                ),
                                ("turn_id".to_string(), serde_json::json!(turn_id)),
                            ])),
                            error: format!("DB write failed: {e}"),
                            error_category: "internal".to_string(),
                            timestamp: Utc::now().to_rfc3339(),
                            retry_count: 0,
                            max_retries: 3,
                            status: "pending".to_string(),
                        };
                        {
                            let mut dlq = server
                                .dead_letters
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            dlq.push_back(dl);
                            while dlq.len() > DLQ_MAX_ENTRIES {
                                dlq.pop_front();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[ingest_event] LLM extraction failed for {conversation_id}:{turn_id}: {e} — releasing claim for retry");
                let _ = server.with_store_for_scope(target_db, |store| {
                    store
                        .release_event_claim(&eh, "ingest")
                        .map_err(|e| format!("{e}"))
                });
                let dl = DeadLetter {
                    id: uuid::Uuid::new_v4().to_string(),
                    tool_name: "ingest_event".to_string(),
                    arguments: Some(serde_json::Map::from_iter([
                        (
                            "conversation_id".to_string(),
                            serde_json::json!(conversation_id),
                        ),
                        ("turn_id".to_string(), serde_json::json!(turn_id)),
                    ])),
                    error: format!("LLM extraction failed: {e}"),
                    error_category: "internal".to_string(),
                    timestamp: Utc::now().to_rfc3339(),
                    retry_count: 0,
                    max_retries: 3,
                    status: "pending".to_string(),
                };
                {
                    let mut dlq = server
                        .dead_letters
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    dlq.push_back(dl);
                    while dlq.len() > DLQ_MAX_ENTRIES {
                        dlq.pop_front();
                    }
                }
            }
        }
    });
    Ok(serde_json::to_string(&serde_json::json!({
        "status": "ingestion queued",
        "hash": event_hash
    }))
    .unwrap())
}

pub(super) async fn handle_get_pipeline_status(server: &MemoryServer) -> Result<String, String> {
    let global_stats = server.with_global_store(|store| {
        store
            .stats(false)
            .map_err(|e| format!("Failed to get global stats: {}", e))
    })?;

    let project_stats = if server.project_db_path.is_some() {
        Some(server.with_project_store(|store| {
            store
                .stats(false)
                .map_err(|e| format!("Failed to get project stats: {}", e))
        })?)
    } else {
        None
    };

    let mut total_entries = global_stats.total;
    let mut by_scope = global_stats.by_scope;
    let mut by_category = global_stats.by_category;

    if let Some(project_stats) = project_stats {
        total_entries += project_stats.total;
        for (k, v) in project_stats.by_scope {
            *by_scope.entry(k).or_insert(0) += v;
        }
        for (k, v) in project_stats.by_category {
            *by_category.entry(k).or_insert(0) += v;
        }
    }

    let cache_size = server
        .tool_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    let hits = server.cache_hits.load(std::sync::atomic::Ordering::Relaxed);
    let misses = server
        .cache_misses
        .load(std::sync::atomic::Ordering::Relaxed);

    let (dlq_total, dlq_pending, dlq_resolved, dlq_abandoned) = {
        let dlq = server
            .dead_letters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let total = dlq.len();
        let pending = dlq.iter().filter(|d| d.status == "pending").count();
        let resolved = dlq.iter().filter(|d| d.status == "resolved").count();
        let abandoned = dlq.iter().filter(|d| d.status == "abandoned").count();
        (total, pending, resolved, abandoned)
    };

    serde_json::to_string(&json!({
        "status": "running",
        "workers": if server.pipeline_enabled { "rust_async" } else { "disabled" },
        "total_entries": total_entries,
        "by_scope": by_scope,
        "by_category": by_category,
        "vec_available": {
            "global": server.global_vec_available,
            "project": server.project_vec_available,
        },
        "pipeline_enabled": server.pipeline_enabled,
        "phantom_tools": {
            "cache_size": cache_size,
            "cache_hits": hits,
            "cache_misses": misses,
            "ttl_seconds": TOOL_CACHE_TTL.as_secs(),
        },
        "dead_letter_queue": {
            "total": dlq_total,
            "pending": dlq_pending,
            "resolved": dlq_resolved,
            "abandoned": dlq_abandoned,
            "max_entries": DLQ_MAX_ENTRIES,
            "ttl_seconds": DLQ_TTL_SECS,
        },
    }))
    .map_err(|e| format!("Failed to serialize response: {}", e))
}

pub(super) async fn handle_sync_memories(
    server: &MemoryServer,
    params: SyncMemoriesParams,
) -> Result<String, String> {
    let path_prefix = params.path_prefix.as_deref().unwrap_or("/");
    let limit = params.limit.min(500);

    let mut all_entries: Vec<(MemoryEntry, DbScope)> = Vec::new();

    let global_entries = server.with_global_store(|store| {
        store
            .list_by_path(path_prefix, limit, false)
            .map_err(|e| format!("Failed to list global memories: {}", e))
    })?;
    all_entries.extend(global_entries.into_iter().map(|e| (e, DbScope::Global)));

    if server.project_db_path.is_some() {
        let project_entries = server.with_project_store(|store| {
            store
                .list_by_path(path_prefix, limit, false)
                .map_err(|e| format!("Failed to list project memories: {}", e))
        })?;
        all_entries.extend(project_entries.into_iter().map(|e| (e, DbScope::Project)));
    }

    all_entries.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));
    all_entries.truncate(limit);

    if all_entries.is_empty() {
        return serde_json::to_string(&json!({
            "agent_id": params.agent_id,
            "new_count": 0,
            "changed_count": 0,
            "entries": [],
        }))
        .map_err(|e| format!("Failed to serialize: {}", e));
    }

    let memory_ids: Vec<String> = all_entries.iter().map(|(e, _)| e.id.clone()).collect();

    let known_revisions = server.with_global_store(|store| {
        store
            .get_agent_known_revisions(&params.agent_id, &memory_ids)
            .map_err(|e| format!("Failed to get known revisions: {}", e))
    })?;

    let mut diff_entries: Vec<serde_json::Value> = Vec::new();
    let mut sync_updates: Vec<(String, i64)> = Vec::new();
    let mut new_count = 0u64;
    let mut changed_count = 0u64;

    for (entry, db_scope) in &all_entries {
        let current_rev = entry.revision;
        match known_revisions.get(&entry.id) {
            None => {
                let mut obj = match slim_entry(entry, *db_scope) {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                };
                obj.insert("diff_type".into(), json!("new"));
                diff_entries.push(serde_json::Value::Object(obj));
                sync_updates.push((entry.id.clone(), current_rev));
                new_count += 1;
            }
            Some(&known_rev) if current_rev > known_rev => {
                let mut obj = match slim_entry(entry, *db_scope) {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                };
                obj.insert("diff_type".into(), json!("changed"));
                obj.insert("prev_revision".into(), json!(known_rev));
                diff_entries.push(serde_json::Value::Object(obj));
                sync_updates.push((entry.id.clone(), current_rev));
                changed_count += 1;
            }
            _ => {
                sync_updates.push((entry.id.clone(), current_rev));
            }
        }
    }

    if !sync_updates.is_empty() {
        server
            .with_global_store(|store| {
                store
                    .update_agent_known_state(&params.agent_id, &sync_updates)
                    .map_err(|e| format!("Failed to update agent state: {}", e))
            })
            .map_err(|e| {
                format!(
                    "sync_memories failed to persist agent state for '{}': {}",
                    params.agent_id, e
                )
            })?;
    }

    serde_json::to_string(&json!({
        "agent_id": params.agent_id,
        "new_count": new_count,
        "changed_count": changed_count,
        "unchanged_count": all_entries.len() as u64 - new_count - changed_count,
        "entries": diff_entries,
    }))
    .map_err(|e| format!("Failed to serialize: {}", e))
}
