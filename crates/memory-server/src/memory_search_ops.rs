use super::*;

pub(super) async fn handle_save_memory(
    server: &MemoryServer,
    params: SaveMemoryParams,
) -> Result<String, String> {
    if !params.force && memory_core::is_noise_text(&params.text) {
        return serde_json::to_string(&json!({
            "saved": false,
            "noise": true,
            "reason": "Text detected as noise (greeting, denial, or meta-question). Not saved.",
            "hint": "Retry with force=true if this is intentional content.",
        }))
        .map_err(|e| format!("Failed to serialize: {}", e));
    }

    let id = params
        .id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let timestamp = Utc::now().to_rfc3339();
    let requested_scope = params.scope.clone();
    let named_project = params.project.clone();
    let (target_db, warning) = if named_project.is_some() {
        (DbScope::Project, None) // Will use named project below
    } else {
        server.resolve_write_scope(&requested_scope)
    };

    let summary = params.summary;
    let needs_summary = summary.is_empty();
    let needs_embedding = params.vector.is_none();
    let path = params.path;
    let category = params.category;
    let topic = params.topic;
    let metadata = crate::provenance::inject_provenance(
        server,
        json!({}),
        "save_memory",
        "memory_write",
        Some(requested_scope.as_str()),
        target_db,
        json!({
            "path": path.clone(),
            "category": category.clone(),
            "topic": topic.clone(),
        }),
    );

    let entry = MemoryEntry {
        id: id.clone(),
        path,
        summary,
        text: params.text,
        importance: params.importance,
        timestamp: timestamp.clone(),
        category,
        topic,
        keywords: params.keywords,
        persons: params.persons,
        entities: params.entities,
        location: params.location,
        source: "mcp".to_string(),
        scope: requested_scope,
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata,
        vector: params.vector,
    };

    if let Some(ref project_name) = named_project {
        server.with_named_project_store(project_name, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory to '{}': {}", project_name, e))
        })?;
    } else {
        server.with_store_for_scope(target_db, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory: {}", e))
        })?;
    }

    // Queue enrichment (embedding + summary) via the batcher instead of
    // spawning a per-item task. The batcher accumulates items and calls
    // the Voyage API in batch (up to 128 per request), dramatically
    // reducing API calls when the agent saves multiple memories in sequence.
    if needs_embedding || needs_summary {
        let _ = server.enrich_tx.send(super::EnrichmentItem {
            id: id.clone(),
            text: entry.text.clone(),
            needs_embedding,
            needs_summary,
            target_db,
            named_project: params.project.clone(),
            foundry_agent_id: None,
            foundry_path_prefix: None,
            revision: 1,
        });
    }

    let mut response = serde_json::Map::new();
    response.insert("id".into(), json!(id));
    response.insert("timestamp".into(), json!(timestamp));
    response.insert("db".into(), json!(target_db.as_str()));
    response.insert("status".into(), json!("saved (enrichment pending)"));
    if let Some(warning) = warning {
        response.insert("warning".into(), json!(warning));
    }

    if params.auto_link && !entry.entities.is_empty() {
        let auto_link_server = server.clone();
        let auto_link_id = id.clone();
        let auto_link_entities = entry.entities.clone();
        let auto_link_named_project = params.project.clone();
        let auto_link_target_db = target_db;

        tokio::spawn(async move {
            for entity in &auto_link_entities {
                let query = entity.clone();
                let search_action = |store: &mut MemoryStore| {
                    store
                        .search(
                            &query,
                            Some(memory_core::SearchOptions {
                                top_k: 5,
                                ..Default::default()
                            }),
                        )
                        .map_err(|e| format!("{}", e))
                };

                let search_res = if let Some(ref p) = auto_link_named_project {
                    auto_link_server.with_named_project_store_read(p, search_action)
                } else {
                    auto_link_server.with_store_for_scope_read(auto_link_target_db, search_action)
                };

                if let Ok(results) = search_res {
                    for result in results {
                        if result.entry.id == auto_link_id {
                            continue;
                        }
                        let shared: Vec<&String> = result
                            .entry
                            .entities
                            .iter()
                            .filter(|e| auto_link_entities.contains(e))
                            .collect();
                        if !shared.is_empty() {
                            let edge = memory_core::MemoryEdge {
                                source_id: auto_link_id.clone(),
                                target_id: result.entry.id.clone(),
                                relation: "related_to".to_string(),
                                weight: 0.5,
                                metadata: json!({ "auto_link": true, "shared_entities": shared }),
                                created_at: chrono::Utc::now().to_rfc3339(),
                                valid_from: String::new(),
                                valid_to: None,
                            };
                            let save_edge_action = |store: &mut MemoryStore| {
                                store.add_edge(&edge).map_err(|e| format!("{}", e))
                            };
                            let _ = if let Some(ref p) = auto_link_named_project {
                                auto_link_server.with_named_project_store(p, save_edge_action)
                            } else {
                                auto_link_server
                                    .with_store_for_scope(auto_link_target_db, save_edge_action)
                            };
                        }
                    }
                }
            }
        });
        response.insert("auto_link".into(), json!("pending"));
    }

    serde_json::to_string(&serde_json::Value::Object(response))
        .map_err(|e| format!("Failed to serialize response: {}", e))
}

pub(super) async fn search_memory_rows(
    server: &MemoryServer,
    mut params: SearchMemoryParams,
) -> Result<Vec<serde_json::Value>, String> {
    if memory_core::should_skip_query(&params.query) {
        return Ok(vec![]);
    }

    if params.query_vec.is_none() && (server.global_vec_available || server.project_vec_available) {
        match server.llm.embed_voyage(&params.query, "query").await {
            Ok(query_vec) => {
                params.query_vec = Some(query_vec);
            }
            Err(e) => {
                eprintln!(
                    "[search_memory] query embedding failed, falling back to lexical-only search: {e}"
                );
            }
        }
    }

    let pipeline_enabled = server.pipeline_enabled;

    let mut combined_results: Vec<(memory_core::SearchResult, DbScope)> = Vec::new();

    let global_opts = params.to_search_options(server.global_vec_available);

    let global_results = server.with_global_store_read(|store| {
        store
            .search(&params.query, Some(global_opts))
            .map_err(|e| format!("Search failed in global DB: {}", e))
    })?;
    combined_results.extend(global_results.into_iter().map(|r| (r, DbScope::Global)));

    // Search project DB — either named project or default
    if let Some(ref project_name) = params.project {
        let project_results = server.with_named_project_store_read(project_name, |store| {
            let vec_avail = store.vec_available;
            let project_opts = params.to_search_options(vec_avail);
            store
                .search(&params.query, Some(project_opts))
                .map_err(|e| format!("Search failed in project DB '{}': {}", project_name, e))
        })?;
        combined_results.extend(project_results.into_iter().map(|r| (r, DbScope::Project)));
    } else if server.has_project_db() {
        let project_opts = params.to_search_options(server.project_vec_available);

        let project_results = server.with_project_store_read(|store| {
            store
                .search(&params.query, Some(project_opts))
                .map_err(|e| format!("Search failed in project DB: {}", e))
        })?;
        combined_results.extend(project_results.into_iter().map(|r| (r, DbScope::Project)));
    }

    combined_results.sort_by(|a, b| {
        b.0.score
            .final_score
            .partial_cmp(&a.0.score.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut seen_ids = HashSet::new();
    let mut deduped_results: Vec<(memory_core::SearchResult, DbScope)> = Vec::new();
    for (result, db_scope) in combined_results {
        if seen_ids.insert(result.entry.id.clone()) {
            deduped_results.push((result, db_scope));
        }
        if deduped_results.len() >= params.top_k {
            break;
        }
    }

    // Sandbox filtering: if agent_role is specified, filter out denied entries
    if let Some(ref role) = params.agent_role {
        deduped_results.retain(|(result, db_scope)| {
            let allowed = match db_scope {
                DbScope::Global => server.with_global_store_read(|store| {
                    store
                        .check_sandbox_access(role, &result.entry.path, "read")
                        .map(|(allowed, _)| allowed)
                        .map_err(|e| format!("{e}"))
                }),
                DbScope::Project => {
                    if let Some(ref p) = params.project {
                        server.with_named_project_store_read(p, |store| {
                            store
                                .check_sandbox_access(role, &result.entry.path, "read")
                                .map(|(allowed, _)| allowed)
                                .map_err(|e| format!("{e}"))
                        })
                    } else {
                        server.with_project_store_read(|store| {
                            store
                                .check_sandbox_access(role, &result.entry.path, "read")
                                .map(|(allowed, _)| allowed)
                                .map_err(|e| format!("{e}"))
                        })
                    }
                }
            };
            allowed.unwrap_or(true)
        });
    }

    let mut output: Vec<serde_json::Value> = deduped_results
        .iter()
        .map(|(r, db_scope)| slim_search_result(r, *db_scope))
        .collect();

    if pipeline_enabled {
        let mut existing_ids: HashSet<String> = deduped_results
            .iter()
            .map(|(r, _)| r.entry.id.clone())
            .collect();

        if server.has_project_db() {
            let project_rules = server.with_project_store_read(|store| {
                Ok(store
                    .list_by_path("/behavior/global_rules", 50, false)
                    .unwrap_or_default())
            })?;
            for rule in project_rules {
                if !is_active_global_rule(&rule) {
                    continue;
                }
                if !existing_ids.insert(rule.id.clone()) {
                    continue;
                }
                output.push(slim_l0_rule(&rule, DbScope::Project));
            }
        }

        let global_rules = server.with_global_store_read(|store| {
            Ok(store
                .list_by_path("/behavior/global_rules", 50, false)
                .unwrap_or_default())
        })?;
        for rule in global_rules {
            if !is_active_global_rule(&rule) {
                continue;
            }
            if !existing_ids.insert(rule.id.clone()) {
                continue;
            }
            output.push(slim_l0_rule(&rule, DbScope::Global));
        }
    }

    Ok(output)
}

pub(super) async fn handle_search_memory(
    server: &MemoryServer,
    params: SearchMemoryParams,
) -> Result<String, String> {
    let rows = search_memory_rows(server, params).await?;
    serde_json::to_string(&rows).map_err(|e| format!("Failed to serialize response: {}", e))
}

pub(super) async fn handle_find_similar_memory(
    server: &MemoryServer,
    params: FindSimilarMemoryParams,
) -> Result<String, String> {
    if params.query_vec.is_empty() {
        return serde_json::to_string(&json!([]))
            .map_err(|e| format!("Failed to serialize response: {}", e));
    }

    if params.query_vec.iter().any(|v| !v.is_finite()) {
        return Err("query_vec contains non-finite values".to_string());
    }

    let mut combined_results: Vec<(memory_core::SearchResult, DbScope)> = Vec::new();
    let common_weights = memory_core::HybridWeights {
        semantic: 1.0,
        fts: 0.0,
        symbolic: 0.0,
        decay: 0.0,
    };

    let global_opts = SearchOptions {
        candidates_per_channel: params.candidates_per_channel.max(params.top_k),
        top_k: params.top_k,
        weights: common_weights.clone(),
        path_prefix: params.path_prefix.clone(),
        query_vec: Some(params.query_vec.clone()),
        vec_available: server.global_vec_available,
        record_access: false,
        include_archived: params.include_archived,
        mmr_threshold: None,
        graph_expand_hops: 0,
        graph_relation_filter: None,
    };

    let global_results = server.with_global_store_read(|store| {
        store
            .search("", Some(global_opts))
            .map_err(|e| format!("Vector search failed in global DB: {}", e))
    })?;
    combined_results.extend(global_results.into_iter().map(|r| (r, DbScope::Global)));

    if server.has_project_db() {
        let project_opts = SearchOptions {
            candidates_per_channel: params.candidates_per_channel.max(params.top_k),
            top_k: params.top_k,
            weights: common_weights,
            path_prefix: params.path_prefix.clone(),
            query_vec: Some(params.query_vec.clone()),
            vec_available: server.project_vec_available,
            record_access: false,
            include_archived: params.include_archived,
            mmr_threshold: None,
            graph_expand_hops: 0,
            graph_relation_filter: None,
        };

        let project_results = server.with_project_store_read(|store| {
            store
                .search("", Some(project_opts))
                .map_err(|e| format!("Vector search failed in project DB: {}", e))
        })?;
        combined_results.extend(project_results.into_iter().map(|r| (r, DbScope::Project)));
    }

    combined_results.sort_by(|a, b| {
        b.0.score
            .vector
            .partial_cmp(&a.0.score.vector)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut seen_ids = HashSet::new();
    let mut output: Vec<serde_json::Value> = Vec::new();
    for (result, db_scope) in combined_results {
        if !seen_ids.insert(result.entry.id.clone()) {
            continue;
        }
        let mut obj = match slim_entry(&result.entry, db_scope) {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        obj.insert(
            "similarity".into(),
            json!((result.score.vector * 1000.0).round() / 1000.0),
        );
        output.push(serde_json::Value::Object(obj));
        if output.len() >= params.top_k {
            break;
        }
    }

    serde_json::to_string(&output).map_err(|e| format!("Failed to serialize response: {}", e))
}
