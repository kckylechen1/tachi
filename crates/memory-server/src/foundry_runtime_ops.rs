use super::*;
use serde::Deserialize;

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
    let raw_results = handle_search_memory(
        server,
        SearchMemoryParams {
            query: params.query.clone(),
            query_vec: None,
            top_k: candidate_top_k,
            path_prefix: params.path_prefix.clone(),
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

    let excluded_topics = params
        .exclude_topics
        .iter()
        .map(|topic| topic.trim().to_ascii_lowercase())
        .filter(|topic| !topic.is_empty())
        .collect::<HashSet<_>>();

    let min_score = params.min_score.unwrap_or(0.0);
    let parsed: Vec<Value> = serde_json::from_str(&raw_results)
        .map_err(|e| format!("Failed to parse search_memory results for recall_context: {e}"))?;

    let filtered = parsed
        .into_iter()
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

    let saved_ids = if let Some(ref project_name) = named_project {
        server.with_named_project_store(project_name, |store| {
            let mut ids = Vec::with_capacity(entries.len());
            for entry in &entries {
                store.upsert(entry).map_err(|e| {
                    format!("Failed to save session capture to '{project_name}': {e}")
                })?;
                ids.push(entry.id.clone());
            }
            Ok(ids)
        })?
    } else {
        server.with_store_for_scope(target_db, |store| {
            let mut ids = Vec::with_capacity(entries.len());
            for entry in &entries {
                store
                    .upsert(entry)
                    .map_err(|e| format!("Failed to save captured memory: {e}"))?;
                ids.push(entry.id.clone());
            }
            Ok(ids)
        })?
    };

    if embeddings.is_none() {
        for entry in &entries {
            let _ = server.enrich_tx.send(EnrichmentItem {
                id: entry.id.clone(),
                text: entry.text.clone(),
                needs_embedding: true,
                needs_summary: false,
                target_db,
                named_project: named_project.clone(),
                revision: entry.revision,
            });
        }
    }

    let mut response = serde_json::Map::new();
    response.insert("status".into(), json!("completed"));
    response.insert("captured".into(), json!(saved_ids.len()));
    response.insert("ids".into(), json!(saved_ids));
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
}
