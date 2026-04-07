use super::*;

pub(super) const DLQ_MAX_ENTRIES: usize = 200;
pub(super) const DLQ_TTL_SECS: u64 = 3600;

#[derive(Clone)]
pub(super) struct DeadLetter {
    pub(super) id: String,
    pub(super) tool_name: String,
    pub(super) arguments: Option<serde_json::Map<String, serde_json::Value>>,
    pub(super) error: String,
    pub(super) error_category: String,
    pub(super) timestamp: String,
    pub(super) retry_count: u32,
    pub(super) max_retries: u32,
    pub(super) status: String,
}

pub(super) fn categorize_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("not found") || lower.contains("not_found") {
        "not_found".to_string()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".to_string()
    } else if lower.contains("invalid") || lower.contains("param") {
        "invalid_params".to_string()
    } else {
        "internal".to_string()
    }
}

pub(super) fn slim_entry(e: &MemoryEntry, db: DbScope) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(e.id));
    obj.insert("db".into(), json!(db.as_str()));
    obj.insert("text".into(), json!(e.text));
    if !e.summary.is_empty() {
        obj.insert("summary".into(), json!(e.summary));
    }
    obj.insert("path".into(), json!(e.path));
    if !e.topic.is_empty() {
        obj.insert("topic".into(), json!(e.topic));
    }
    if !e.keywords.is_empty() {
        obj.insert("keywords".into(), json!(e.keywords));
    }
    obj.insert("importance".into(), json!(e.importance));
    obj.insert("timestamp".into(), json!(e.timestamp));
    obj.insert("category".into(), json!(e.category));
    obj.insert("scope".into(), json!(e.scope));
    if !e.persons.is_empty() {
        obj.insert("persons".into(), json!(e.persons));
    }
    if !e.entities.is_empty() {
        obj.insert("entities".into(), json!(e.entities));
    }
    if !e.location.is_empty() {
        obj.insert("location".into(), json!(e.location));
    }
    if let Some(ref rp) = e.retention_policy {
        obj.insert("retention_policy".into(), json!(rp));
    }
    if let Some(ref domain) = e.domain {
        if !domain.is_empty() {
            obj.insert("domain".into(), json!(domain));
        }
    }
    if e.archived {
        obj.insert("archived".into(), json!(true));
    }
    if let serde_json::Value::Object(ref m) = e.metadata {
        if !m.is_empty() {
            obj.insert("metadata".into(), json!(m));
        }
    }
    serde_json::Value::Object(obj)
}

pub(super) fn slim_search_result(
    result: &memory_core::SearchResult,
    db: DbScope,
) -> serde_json::Value {
    let mut obj = match slim_entry(&result.entry, db) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(
        "relevance".into(),
        json!((result.score.final_score * 1000.0).round() / 1000.0),
    );
    obj.insert(
        "score".into(),
        json!({
            "vector": (result.score.vector * 1000.0).round() / 1000.0,
            "fts": (result.score.fts * 1000.0).round() / 1000.0,
            "symbolic": (result.score.symbolic * 1000.0).round() / 1000.0,
            "decay": (result.score.decay * 1000.0).round() / 1000.0,
            "final": (result.score.final_score * 1000.0).round() / 1000.0,
        }),
    );
    serde_json::Value::Object(obj)
}

pub(super) fn slim_l0_rule(rule: &MemoryEntry, db: DbScope) -> serde_json::Value {
    let mut obj = match slim_entry(rule, db) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("l0_rule".into(), json!(true));
    serde_json::Value::Object(obj)
}
