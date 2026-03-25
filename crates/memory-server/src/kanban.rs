use super::*;

pub(super) const KANBAN_CATEGORY: &str = "kanban";
pub(super) const KANBAN_PATH_PREFIX: &str = "/kanban/";

fn default_card_priority() -> String {
    "medium".to_string()
}

fn default_card_type() -> String {
    "request".to_string()
}

fn default_include_broadcast() -> bool {
    true
}

fn default_inbox_limit() -> usize {
    100
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PostCardParams {
    /// Source agent ID (e.g. "hapi")
    pub from_agent: String,
    /// Destination agent ID, or "*" for broadcast
    pub to_agent: String,
    /// Short card title
    pub title: String,
    /// Card body content
    pub body: String,
    /// Priority: low | medium | high | critical
    #[serde(default = "default_card_priority")]
    pub priority: String,
    /// Card type: request | report | alert | handoff
    #[serde(default = "default_card_type")]
    pub card_type: String,
    /// Optional thread correlation ID
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct CheckInboxParams {
    /// Agent ID receiving cards
    pub agent_id: String,
    /// Optional status filter (e.g. "open")
    #[serde(default)]
    pub status_filter: Option<String>,
    /// Optional ISO timestamp lower bound (inclusive)
    #[serde(default)]
    pub since: Option<String>,
    /// Include broadcast cards addressed to "*"
    #[serde(default = "default_include_broadcast")]
    pub include_broadcast: bool,
    /// Maximum cards returned
    #[serde(default = "default_inbox_limit")]
    pub limit: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct UpdateCardParams {
    /// Kanban card memory ID
    pub card_id: String,
    /// New status: open | acknowledged | resolved | expired
    pub new_status: String,
    /// Optional threaded response appended to the card
    #[serde(default)]
    pub response_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KanbanClassification {
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    priority_suggestion: Option<String>,
}

pub(super) fn normalize_agent_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

pub(super) fn normalize_card_priority(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => "low".to_string(),
        "medium" => "medium".to_string(),
        "high" => "high".to_string(),
        "critical" => "critical".to_string(),
        _ => default_card_priority(),
    }
}

pub(super) fn normalize_card_type(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "request" => "request".to_string(),
        "report" => "report".to_string(),
        "alert" => "alert".to_string(),
        "handoff" => "handoff".to_string(),
        _ => default_card_type(),
    }
}

pub(super) fn normalize_card_status(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "open" => Some("open".to_string()),
        "acknowledged" => Some("acknowledged".to_string()),
        "resolved" => Some("resolved".to_string()),
        "expired" => Some("expired".to_string()),
        _ => None,
    }
}

pub(super) fn kanban_priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        _ => 3,
    }
}

pub(super) fn kanban_priority_importance(priority: &str) -> f64 {
    match priority {
        "critical" => 1.0,
        "high" => 0.9,
        "medium" => 0.75,
        _ => 0.6,
    }
}

pub(super) fn card_metadata_str(entry: &MemoryEntry, key: &str) -> Option<String> {
    entry
        .metadata
        .get(key)
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

pub(super) fn card_to_agent(entry: &MemoryEntry) -> Option<String> {
    card_metadata_str(entry, "to_agent")
        .map(|v| normalize_agent_id(&v))
        .or_else(|| {
            entry
                .path
                .strip_prefix(KANBAN_PATH_PREFIX)
                .and_then(|rest| rest.split('/').nth(1))
                .map(normalize_agent_id)
        })
}

pub(super) fn card_from_agent(entry: &MemoryEntry) -> Option<String> {
    card_metadata_str(entry, "from_agent")
        .map(|v| normalize_agent_id(&v))
        .or_else(|| {
            entry
                .path
                .strip_prefix(KANBAN_PATH_PREFIX)
                .and_then(|rest| rest.split('/').next())
                .map(normalize_agent_id)
        })
}

pub(super) fn card_status(entry: &MemoryEntry) -> Option<String> {
    card_metadata_str(entry, "status").map(|v| v.to_ascii_lowercase())
}

pub(super) fn card_priority(entry: &MemoryEntry) -> String {
    card_metadata_str(entry, "priority")
        .map(|v| normalize_card_priority(&v))
        .unwrap_or_else(default_card_priority)
}

pub(super) fn card_type(entry: &MemoryEntry) -> String {
    card_metadata_str(entry, "card_type")
        .map(|v| normalize_card_type(&v))
        .unwrap_or_else(default_card_type)
}

pub(super) fn card_matches_inbox(
    entry: &MemoryEntry,
    params: &CheckInboxParams,
    agent_id: &str,
) -> bool {
    if entry.category != KANBAN_CATEGORY || entry.archived {
        return false;
    }
    let to_agent = match card_to_agent(entry) {
        Some(v) => v,
        None => return false,
    };
    if to_agent != agent_id && !(params.include_broadcast && to_agent == "*") {
        return false;
    }
    if let Some(filter) = params
        .status_filter
        .as_ref()
        .and_then(|v| normalize_card_status(v))
    {
        if card_status(entry).as_deref() != Some(filter.as_str()) {
            return false;
        }
    }
    if let Some(since) = params.since.as_ref() {
        if entry.timestamp < *since {
            return false;
        }
    }
    true
}

async fn classify_kanban_message(title: &str, body: &str) -> Result<KanbanClassification, String> {
    let model_url = std::env::var("KANBAN_MODEL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/api/generate".to_string());
    let model_name =
        std::env::var("KANBAN_MODEL_NAME").unwrap_or_else(|_| "qwen2.5:32b".to_string());

    let prompt = format!(
        "Classify this inter-agent message. Return JSON only.\\nTitle: {title}\\nBody: {body}\\nOutput: {{\"topic\":\"...\",\"keywords\":[\"...\"],\"priority_suggestion\":\"...\"}}"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("build classifier client: {e}"))?;

    let response = client
        .post(&model_url)
        .json(&json!({
            "model": model_name,
            "prompt": prompt,
            "stream": false,
            "format": "json"
        }))
        .send()
        .await
        .map_err(|e| format!("kanban classifier request failed: {e}"))?;

    let status = response.status();
    let raw_text = response
        .text()
        .await
        .map_err(|e| format!("kanban classifier response read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("kanban classifier error {status}: {raw_text}"));
    }

    let raw_json: serde_json::Value = serde_json::from_str(&raw_text)
        .map_err(|e| format!("kanban classifier response is not valid JSON: {e}"))?;

    let payload_text = raw_json
        .get("response")
        .and_then(|v| v.as_str())
        .or_else(|| {
            raw_json
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(|v| v.as_str())
        })
        .ok_or_else(|| "classifier payload missing response text".to_string())?;

    let cleaned = llm::LlmClient::strip_code_fence(payload_text)
        .trim()
        .to_string();
    serde_json::from_str::<KanbanClassification>(&cleaned)
        .map_err(|e| format!("failed to parse classifier JSON payload: {e}; raw={cleaned}"))
}

pub(super) async fn enrich_kanban_card_classification(
    db_path: Arc<PathBuf>,
    card_id: String,
    card_text: String,
    card_summary: String,
    card_source: String,
    base_metadata: serde_json::Value,
    expected_revision: i64,
) -> Result<(), String> {
    let classification = classify_kanban_message(&card_summary, &card_text).await?;
    let mut metadata = base_metadata;
    if !metadata.is_object() {
        metadata = json!({});
    }

    if let Some(topic) = classification.topic.as_ref() {
        metadata["topic"] = json!(topic);
    }
    if !classification.keywords.is_empty() {
        metadata["keywords"] = json!(classification.keywords);
    }
    if let Some(priority) = classification.priority_suggestion.as_ref() {
        metadata["priority_suggestion"] = json!(normalize_card_priority(priority));
    }
    metadata["classified_at"] = json!(Utc::now().to_rfc3339());

    let mut store = MemoryStore::open(db_path.to_string_lossy().as_ref())
        .map_err(|e| format!("open db for kanban enrichment: {e}"))?;
    let updated = store
        .update_with_revision(
            &card_id,
            &card_text,
            &card_summary,
            &card_source,
            &metadata,
            None,
            expected_revision,
        )
        .map_err(|e| format!("kanban enrichment update failed: {e}"))?;

    if !updated {
        return Err("kanban enrichment skipped: revision changed".to_string());
    }

    Ok(())
}
