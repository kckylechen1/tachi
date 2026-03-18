use crate::llm::LlmClient;
use crate::prompts::{CAUSAL_PROMPT, CONTRADICTION_PROMPT, DISTILLER_PROMPT, MERGE_PROMPT};
use chrono::Utc;
use memory_core::{HybridWeights, MemoryEdge, MemoryEntry, MemoryStore, SearchOptions};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

fn strip_code_fence(text: &str) -> &str {
    let text = text.trim();
    let text = if text.starts_with("```json") {
        text[7..].trim()
    } else if text.starts_with("```") {
        text[3..].trim()
    } else {
        text
    };
    text.trim_end_matches("```").trim()
}

fn parse_json_array(content: &str) -> Vec<Value> {
    let clean = strip_code_fence(content);
    match serde_json::from_str::<Vec<Value>>(clean) {
        Ok(arr) => arr,
        Err(_) => Vec::new(),
    }
}

fn parse_json_object(content: &str) -> Option<Value> {
    let clean = strip_code_fence(content);
    serde_json::from_str::<Value>(clean)
        .ok()
        .filter(Value::is_object)
}

fn messages_to_text(messages: &[Value]) -> String {
    let mut out = String::new();
    for msg in messages {
        if let Some(obj) = msg.as_object() {
            let role = obj
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .trim();
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !content.is_empty() {
                out.push_str(&format!("{role}: {content}\n"));
            }
        } else if let Some(text) = msg.as_str() {
            let text = text.trim();
            if !text.is_empty() {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

fn extract_entities(entry: &MemoryEntry) -> HashSet<String> {
    let mut entities = HashSet::new();

    for e in entry.entities.iter().chain(entry.persons.iter()) {
        let normalized = e.trim().to_lowercase();
        if normalized.len() >= 2 {
            entities.insert(normalized);
        }
    }

    entities
}

fn find_memory_id(store: &Arc<Mutex<MemoryStore>>, text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }

    let mut opts = SearchOptions {
        top_k: 1,
        record_access: false,
        ..Default::default()
    };
    opts.weights = HybridWeights {
        semantic: 0.0,
        fts: 1.0,
        symbolic: 0.5,
        decay: 0.0,
    };

    let mut guard = match store.lock() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("run_causal: store lock poisoned while searching memory id: {e}");
            return None;
        }
    };

    let results = match guard.search(text, Some(opts)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("run_causal: failed to search memory for '{text}': {e}");
            return None;
        }
    };

    results.into_iter().next().and_then(|r| {
        if r.score.final_score > 0.1 {
            Some(r.entry.id)
        } else {
            None
        }
    })
}

fn parse_distilled_rules(content: &str) -> Vec<String> {
    parse_json_array(content)
        .into_iter()
        .filter_map(|item| {
            if let Some(rule) = item.as_str() {
                let trimmed = rule.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
                return None;
            }

            if let Some(obj) = item.as_object() {
                if let Some(rule) = obj.get("rule").and_then(Value::as_str) {
                    let trimmed = rule.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }

                if let Ok(serialized) = serde_json::to_string(&item) {
                    if !serialized.trim().is_empty() {
                        return Some(serialized);
                    }
                }
            }

            None
        })
        .collect()
}

fn parse_confidence(item: &Value, key: &str, default: f64) -> f64 {
    item.get(key)
        .and_then(Value::as_f64)
        .or_else(|| {
            item.get(key)
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(default)
}

pub async fn run_causal(
    store: Arc<Mutex<MemoryStore>>,
    llm: Arc<LlmClient>,
    messages: Vec<Value>,
    event_id: String,
) {
    if messages.len() < 2 {
        return;
    }

    let conversation_text = messages_to_text(&messages);
    if conversation_text.trim().is_empty() {
        return;
    }

    let content = match llm
        .call_llm_with_model(
            CAUSAL_PROMPT,
            &conversation_text,
            "Qwen/Qwen3.5-27B",
            0.1,
            1000,
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("run_causal: llm call failed: {e}");
            return;
        }
    };

    let items = parse_json_array(&content);
    if items.is_empty() {
        return;
    }

    for item in items {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if item_type == "correction" {
            let context = item
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let wrong_action = item
                .get("wrong_action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let correct_action = item
                .get("correct_action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();

            if context.is_empty() || wrong_action.is_empty() || correct_action.is_empty() {
                continue;
            }

            let text = match serde_json::to_string(&json!({
                "context": context,
                "wrong_action": wrong_action,
                "correct_action": correct_action,
            })) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("run_causal: failed to serialize correction payload: {e}");
                    continue;
                }
            };

            let summary = match llm.generate_summary(&text).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("run_causal: summary generation failed: {e}");
                    text.chars().take(100).collect::<String>()
                }
            };

            let metadata = json!({
                "origin": "causal",
                "event_id": event_id.clone(),
            });

            let save_result = match store.lock() {
                Ok(guard) => guard.save_derived(
                    &text,
                    "/behavior/corrections",
                    &summary,
                    0.9,
                    "causal",
                    "general",
                    &metadata,
                ),
                Err(e) => {
                    eprintln!("run_causal: store lock poisoned while saving derived item: {e}");
                    continue;
                }
            };

            if let Err(e) = save_result {
                eprintln!("run_causal: failed to save derived correction: {e}");
            }

            continue;
        }

        if item_type != "causal" {
            continue;
        }

        let cause_text = item
            .get("cause_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let effect_text = item
            .get("effect_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let relation = item
            .get("relation")
            .and_then(Value::as_str)
            .unwrap_or("causes")
            .trim();
        let confidence = parse_confidence(&item, "confidence", 0.0);

        if cause_text.is_empty() || effect_text.is_empty() || confidence < 0.5 {
            continue;
        }

        let cause_id = find_memory_id(&store, cause_text);
        let effect_id = find_memory_id(&store, effect_text);

        let (Some(source_id), Some(target_id)) = (cause_id, effect_id) else {
            continue;
        };

        if source_id == target_id {
            continue;
        }

        let edge = MemoryEdge {
            source_id,
            target_id,
            relation: if relation.is_empty() {
                "causes".to_string()
            } else {
                relation.to_string()
            },
            weight: confidence.clamp(0.0, 1.0),
            metadata: json!({
                "origin": "causal_worker",
                "event_id": event_id.clone(),
            }),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        };

        let add_result = match store.lock() {
            Ok(guard) => guard.add_edge(&edge),
            Err(e) => {
                eprintln!("run_causal: store lock poisoned while adding edge: {e}");
                continue;
            }
        };

        if let Err(e) = add_result {
            eprintln!("run_causal: failed to add causal edge: {e}");
        }
    }
}

pub async fn run_consolidator(
    store: Arc<Mutex<MemoryStore>>,
    llm: Arc<LlmClient>,
    memory_id: String,
) {
    let new_memory = match store.lock() {
        Ok(guard) => match guard.get(&memory_id) {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("run_consolidator: failed to load memory {memory_id}: {e}");
                return;
            }
        },
        Err(e) => {
            eprintln!("run_consolidator: store lock poisoned while loading memory {memory_id}: {e}");
            return;
        }
    };

    let Some(new_memory) = new_memory else {
        return;
    };

    let new_text = new_memory.text.trim().to_string();
    if new_text.is_empty() {
        return;
    }

    let query_vec = match llm.embed_voyage(&new_text, "query").await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("run_consolidator: failed to embed query memory {memory_id}: {e}");
            return;
        }
    };

    let mut opts = SearchOptions {
        top_k: 10,
        path_prefix: Some(new_memory.path.clone()),
        query_vec: Some(query_vec),
        record_access: false,
        include_archived: false,
        ..Default::default()
    };
    opts.weights = HybridWeights {
        semantic: 1.0,
        fts: 0.0,
        symbolic: 0.0,
        decay: 0.0,
    };

    let candidates = match store.lock() {
        Ok(mut guard) => match guard.search("", Some(opts)) {
            Ok(results) => results,
            Err(e) => {
                eprintln!("run_consolidator: similarity search failed for {memory_id}: {e}");
                return;
            }
        },
        Err(e) => {
            eprintln!("run_consolidator: store lock poisoned while searching candidates: {e}");
            return;
        }
    };

    for candidate in candidates {
        if candidate.entry.id == memory_id {
            continue;
        }

        let score = candidate.score.final_score;

        if score > 0.85 {
            let old_text = candidate.entry.text.trim().to_string();
            if old_text.is_empty() {
                continue;
            }

            let merge_input = format!("旧记忆:\n{old_text}\n\n新记忆:\n{new_text}");
            let merged_text = match llm.call_llm(MERGE_PROMPT, &merge_input, None, 0.1, 800).await {
                Ok(t) => t.trim().to_string(),
                Err(e) => {
                    eprintln!("run_consolidator: merge llm call failed ({memory_id} -> {}): {e}", candidate.entry.id);
                    continue;
                }
            };

            if merged_text.is_empty() {
                continue;
            }

            let merged_summary = match llm.generate_summary(&merged_text).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("run_consolidator: summary generation failed during merge ({memory_id} -> {}): {e}", candidate.entry.id);
                    merged_text.chars().take(100).collect()
                }
            };

            let merged_vec = match llm.embed_voyage(&merged_text, "document").await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("run_consolidator: failed to embed merged text ({memory_id} -> {}): {e}", candidate.entry.id);
                    continue;
                }
            };

            let target_id = candidate.entry.id.clone();
            let merge_ok = match store.lock() {
                Ok(mut guard) => {
                    let target_entry = match guard.get(&target_id) {
                        Ok(Some(entry)) => entry,
                        Ok(None) => {
                            eprintln!("run_consolidator: target memory disappeared during merge: {target_id}");
                            continue;
                        }
                        Err(e) => {
                            eprintln!("run_consolidator: failed to reload target memory {target_id}: {e}");
                            continue;
                        }
                    };

                    let mut merged_entry = target_entry;
                    merged_entry.text = merged_text.clone();
                    merged_entry.summary = merged_summary.clone();
                    merged_entry.source = "consolidation".to_string();
                    merged_entry.vector = Some(merged_vec.clone());

                    if !merged_entry.metadata.is_object() {
                        merged_entry.metadata = json!({});
                    }
                    if let Some(meta) = merged_entry.metadata.as_object_mut() {
                        meta.insert("merged_from".to_string(), json!(memory_id.clone()));
                        meta.insert("merge_source".to_string(), json!("consolidator"));
                    }

                    if let Err(e) = guard.upsert(&merged_entry) {
                        eprintln!("run_consolidator: failed to upsert merged memory {target_id}: {e}");
                        false
                    } else if let Err(e) = guard.archive_memory(&memory_id) {
                        eprintln!("run_consolidator: failed to archive merged source {memory_id}: {e}");
                        false
                    } else {
                        true
                    }
                }
                Err(e) => {
                    eprintln!("run_consolidator: store lock poisoned during merge write: {e}");
                    false
                }
            };

            if merge_ok {
                break;
            }

            continue;
        }

        if !(0.5 < score && score <= 0.85) {
            continue;
        }

        let contradiction_input = format!("记忆A:\n{new_text}\n\n记忆B:\n{}", candidate.entry.text);
        let contradiction_raw = match llm
            .call_llm(CONTRADICTION_PROMPT, &contradiction_input, None, 0.0, 200)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "run_consolidator: contradiction check failed ({memory_id} vs {}): {e}",
                    candidate.entry.id
                );
                continue;
            }
        };

        let Some(contradiction) = parse_json_object(&contradiction_raw) else {
            continue;
        };

        let contradicts = contradiction
            .get("contradicts")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !contradicts {
            continue;
        }

        let reason = contradiction
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        let edge = MemoryEdge {
            source_id: memory_id.clone(),
            target_id: candidate.entry.id.clone(),
            relation: "contradicts".to_string(),
            weight: 0.9,
            metadata: json!({
                "reason": reason,
                "source": "consolidator",
            }),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        };

        match store.lock() {
            Ok(guard) => {
                if let Err(e) = guard.add_edge(&edge) {
                    eprintln!("run_consolidator: failed to add contradiction edge: {e}");
                }
            }
            Err(e) => {
                eprintln!("run_consolidator: store lock poisoned while adding contradiction edge: {e}");
            }
        }

        break;
    }

    let entities = extract_entities(&new_memory);
    if entities.is_empty() {
        return;
    }

    for entity in entities.clone() {
        let mut opts = SearchOptions {
            top_k: 5,
            path_prefix: Some(new_memory.path.clone()),
            record_access: false,
            include_archived: false,
            ..Default::default()
        };
        opts.weights = HybridWeights {
            semantic: 0.0,
            fts: 1.0,
            symbolic: 0.0,
            decay: 0.0,
        };

        let candidates = match store.lock() {
            Ok(mut guard) => match guard.search(&entity, Some(opts)) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("run_consolidator: entity search failed for '{entity}': {e}");
                    continue;
                }
            },
            Err(e) => {
                eprintln!("run_consolidator: store lock poisoned during entity search: {e}");
                continue;
            }
        };

        for cand in candidates {
            let cand_id = cand.entry.id.trim().to_string();
            if cand_id.is_empty() || cand_id == memory_id {
                continue;
            }

            let cand_entities = extract_entities(&cand.entry);
            let shared: Vec<String> = entities
                .intersection(&cand_entities)
                .cloned()
                .collect();

            if shared.is_empty() {
                continue;
            }

            let edge = MemoryEdge {
                source_id: memory_id.clone(),
                target_id: cand_id,
                relation: "related_to".to_string(),
                weight: (0.5 + 0.1 * shared.len() as f64).min(0.9),
                metadata: json!({
                    "shared_entities": shared,
                    "source": "consolidator",
                }),
                created_at: String::new(),
                valid_from: String::new(),
                valid_to: None,
            };

            match store.lock() {
                Ok(guard) => {
                    if let Err(e) = guard.add_edge(&edge) {
                        eprintln!("run_consolidator: failed to add related_to edge: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("run_consolidator: store lock poisoned while adding related_to edge: {e}");
                }
            }
        }
    }
}

pub async fn run_distiller(
    store: Arc<Mutex<MemoryStore>>,
    llm: Arc<LlmClient>,
    poll_interval_secs: u64,
) {
    loop {
        let count = match store.lock() {
            Ok(guard) => match guard.count_derived_by_source("causal", "/behavior/corrections") {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("run_distiller: failed to count derived items: {e}");
                    0
                }
            },
            Err(e) => {
                eprintln!("run_distiller: store lock poisoned while counting derived items: {e}");
                0
            }
        };

        if count >= 5 {
            let derived_items = match store.lock() {
                Ok(guard) => match guard.list_derived_by_source("causal", "/behavior/corrections", 2000) {
                    Ok(items) => items,
                    Err(e) => {
                        eprintln!("run_distiller: failed to list derived items: {e}");
                        Vec::new()
                    }
                },
                Err(e) => {
                    eprintln!("run_distiller: store lock poisoned while listing derived items: {e}");
                    Vec::new()
                }
            };

            let sample_text = derived_items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if text.is_empty() {
                        None
                    } else {
                        Some(format!("[{}] {}", idx + 1, text))
                    }
                })
                .collect::<Vec<String>>()
                .join("\n\n");

            if !sample_text.is_empty() {
                let response = match llm
                    .call_llm(DISTILLER_PROMPT, &sample_text, None, 0.1, 2000)
                    .await
                {
                    Ok(r) => Some(r),
                    Err(e) => {
                        eprintln!("run_distiller: llm distillation call failed: {e}");
                        None
                    }
                };

                if let Some(response) = response {
                    let rules = parse_distilled_rules(&response);
                    for rule in rules {
                        let vector = match llm.embed_voyage(&rule, "document").await {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("run_distiller: failed to embed distilled rule: {e}");
                                continue;
                            }
                        };

                        let summary = match llm.generate_summary(&rule).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("run_distiller: summary generation failed for distilled rule: {e}");
                                rule.chars().take(100).collect()
                            }
                        };

                        let entry = MemoryEntry {
                            id: Uuid::new_v4().to_string(),
                            path: "/behavior/global_rules".to_string(),
                            summary,
                            text: rule,
                            importance: 0.95,
                            timestamp: Utc::now().to_rfc3339(),
                            category: "fact".to_string(),
                            topic: "global_rule".to_string(),
                            keywords: vec!["rule".to_string(), "distillation".to_string()],
                            persons: vec![],
                            entities: vec![],
                            location: String::new(),
                            source: "distillation".to_string(),
                            scope: "general".to_string(),
                            archived: false,
                            access_count: 0,
                            last_access: None,
                            metadata: json!({
                                "origin": "distillation",
                                "state": "DRAFT",
                            }),
                            vector: Some(vector),
                        };

                        match store.lock() {
                            Ok(mut guard) => {
                                if let Err(e) = guard.upsert(&entry) {
                                    eprintln!("run_distiller: failed to save distilled rule memory: {e}");
                                }
                            }
                            Err(e) => {
                                eprintln!("run_distiller: store lock poisoned while saving distilled rule: {e}");
                            }
                        }
                    }
                }
            }
        }

        sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}
