use super::*;

#[allow(dead_code)]
fn default_path() -> String {
    "/".to_string()
}

#[allow(dead_code)]
fn default_importance() -> f64 {
    0.7
}

#[allow(dead_code)]
fn default_category() -> String {
    "fact".to_string()
}

#[allow(dead_code)]
fn default_scope() -> String {
    "project".to_string()
}

fn default_auto_link() -> bool {
    true
}

#[allow(dead_code)]
fn default_top_k() -> usize {
    6
}

#[allow(dead_code)]
fn default_candidates() -> usize {
    20
}

#[allow(dead_code)]
fn default_mmr_threshold() -> Option<f64> {
    Some(0.85)
}

fn default_graph_hops() -> u32 {
    1
}

fn default_find_similar_top_k() -> usize {
    5
}

#[allow(dead_code)]
fn default_limit() -> usize {
    100
}

#[allow(dead_code)]
fn default_extraction_source() -> String {
    "extraction".to_string()
}

fn default_edge_weight() -> f64 {
    1.0
}

fn default_edge_direction() -> String {
    "both".to_string()
}

fn default_weight_semantic() -> f64 {
    0.40
}

fn default_weight_fts() -> f64 {
    0.30
}

fn default_weight_symbolic() -> f64 {
    0.20
}

fn default_weight_decay() -> f64 {
    0.10
}

#[allow(dead_code)]
fn default_sync_limit() -> usize {
    100
}

// ─── Save / Update ──────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SaveMemoryParams {
    /// Full text content of the memory
    pub text: String,

    /// Short summary (≤100 chars)
    #[serde(default)]
    pub summary: String,

    /// Hierarchical path, e.g. "/openclaw/agent-main"
    #[serde(default = "default_path")]
    pub path: String,

    /// 0.0–1.0 importance score
    #[serde(default = "default_importance")]
    pub importance: f64,

    /// Category: "fact" | "decision" | "experience" | "preference" | "entity" | "other"
    #[serde(default = "default_category")]
    pub category: String,

    /// Topic / subject area
    #[serde(default)]
    pub topic: String,

    /// Keyword tags
    #[serde(default)]
    pub keywords: Vec<String>,

    /// Person names mentioned
    #[serde(default)]
    pub persons: Vec<String>,

    /// Entity names mentioned
    #[serde(default)]
    pub entities: Vec<String>,

    /// Physical or logical location
    #[serde(default)]
    pub location: String,

    /// Scope: "user" | "project" | "general"
    #[serde(default = "default_scope")]
    pub scope: String,

    /// Optional embedding vector (if provided, skip embedding generation)
    #[serde(default)]
    pub vector: Option<Vec<f32>>,

    /// Optional entry ID (for updates)
    #[serde(default)]
    pub id: Option<String>,

    /// Bypass noise filter and force-save text
    #[serde(default)]
    pub force: bool,

    /// Auto-link: create graph edges to memories sharing the same entities (default: true)
    #[serde(default = "default_auto_link")]
    pub auto_link: bool,

    /// Optional project name to target a specific project DB (e.g. "hapi", "sigil").
    /// If omitted, uses the default project DB configured at startup.
    #[serde(default)]
    pub project: Option<String>,

    /// Retention policy: "ephemeral" | "durable" | "permanent" | "pinned".
    /// NULL/omitted = durable (default).
    #[serde(default)]
    pub retention_policy: Option<String>,

    /// Domain this memory belongs to (e.g. "finance", "code-review").
    /// NULL means no domain scoping.
    #[serde(default)]
    pub domain: Option<String>,
}

// ─── Search ─────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HybridWeightsParam {
    /// Semantic (vector) weight (default: 0.4)
    #[serde(default = "default_weight_semantic")]
    pub semantic: f64,
    /// Full-text search weight (default: 0.3)
    #[serde(default = "default_weight_fts")]
    pub fts: f64,
    /// Symbolic weight (default: 0.2)
    #[serde(default = "default_weight_symbolic")]
    pub symbolic: f64,
    /// Decay weight (default: 0.1)
    #[serde(default = "default_weight_decay")]
    pub decay: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SearchMemoryParams {
    /// Search query text
    pub query: String,

    /// Optional query embedding vector; when provided, enables vector channel
    #[serde(default)]
    pub query_vec: Option<Vec<f32>>,

    /// Number of results to return (default: 6)
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Optional path prefix filter
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Whether to include archived entries
    #[serde(default)]
    pub include_archived: bool,

    /// Number of candidates per channel
    #[serde(default = "default_candidates")]
    pub candidates_per_channel: usize,

    /// MMR diversity threshold (0.0-1.0), set to null to disable
    #[serde(default = "default_mmr_threshold")]
    pub mmr_threshold: Option<f64>,

    /// Graph expand hops (0 = disabled, default = 1)
    #[serde(default = "default_graph_hops")]
    pub graph_expand_hops: u32,

    /// Optional relation filter for graph expansion
    #[serde(default)]
    pub graph_relation_filter: Option<String>,

    /// Optional scoring weights override {semantic, fts, symbolic, decay}
    #[serde(default)]
    pub weights: Option<HybridWeightsParam>,

    /// Optional agent role for sandbox filtering (e.g. "finance", "code-review")
    #[serde(default)]
    pub agent_role: Option<String>,

    /// Optional project name to search a specific project DB (e.g. "hapi", "sigil").
    /// If omitted, searches the default project DB configured at startup.
    #[serde(default)]
    pub project: Option<String>,

    /// Domain filter — only return memories belonging to this domain.
    /// NULL means no domain filtering.
    #[serde(default)]
    pub domain: Option<String>,
}

impl SearchMemoryParams {
    /// Build SearchOptions from params, only differing by vec_available per DB.
    pub(crate) fn to_search_options(&self, vec_available: bool) -> SearchOptions {
        let weights = match &self.weights {
            Some(w) => HybridWeights {
                semantic: w.semantic,
                fts: w.fts,
                symbolic: w.symbolic,
                decay: w.decay,
            },
            None => HybridWeights::default(),
        };
        SearchOptions {
            top_k: self.top_k,
            path_prefix: self.path_prefix.clone(),
            query_vec: self.query_vec.clone(),
            include_archived: self.include_archived,
            candidates_per_channel: self.candidates_per_channel,
            mmr_threshold: self.mmr_threshold,
            graph_expand_hops: self.graph_expand_hops,
            graph_relation_filter: self.graph_relation_filter.clone(),
            vec_available,
            weights,
            // Keep search path read-only so multiple search requests can run concurrently.
            record_access: false,
            domain: self.domain.clone(),
            ..Default::default()
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct FindSimilarMemoryParams {
    /// Query embedding vector (same dimension as stored embeddings)
    pub query_vec: Vec<f32>,

    /// Number of results to return (default: 5)
    #[serde(default = "default_find_similar_top_k")]
    pub top_k: usize,

    /// Optional path prefix filter
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Whether to include archived entries
    #[serde(default)]
    pub include_archived: bool,

    /// Number of candidates pulled from vector channel (default: 20)
    #[serde(default = "default_candidates")]
    pub candidates_per_channel: usize,
}

// ─── Get / List / Delete / Archive ──────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetMemoryParams {
    /// Memory entry ID
    pub id: String,

    /// Whether to include archived entries
    #[serde(default)]
    pub include_archived: bool,

    /// Optional project name to search a specific project DB (e.g. "hapi", "sigil").
    /// If omitted, searches the default project DB configured at startup.
    #[serde(default)]
    pub project: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListMemoriesParams {
    /// Path prefix to filter
    #[serde(default = "default_path")]
    pub path_prefix: String,

    /// Maximum number of entries to return
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Whether to include archived entries
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct DeleteMemoryParams {
    /// Memory entry ID to delete
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ArchiveMemoryParams {
    /// Memory entry ID to archive
    pub id: String,
}

// ─── Graph Edges ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct AddEdgeParams {
    /// Source memory ID
    pub source_id: String,
    /// Target memory ID
    pub target_id: String,
    /// Relation type (e.g. "causes", "follows", "related_to")
    pub relation: String,
    /// Edge weight (default: 1.0)
    #[serde(default = "default_edge_weight")]
    pub weight: f64,
    /// Optional JSON metadata for the edge
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,

    /// Scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    pub scope: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetEdgesParams {
    /// Memory entry ID
    pub memory_id: String,
    /// Direction: "outgoing", "incoming", or "both" (default: "both")
    #[serde(default = "default_edge_direction")]
    pub direction: String,
    /// Optional relation type filter
    #[serde(default)]
    pub relation_filter: Option<String>,

    /// Scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    pub scope: String,
}

// ─── Sync ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SyncMemoriesParams {
    /// Unique agent identifier for tracking known state
    pub agent_id: String,
    /// Optional path prefix to scope the sync (e.g. "/project")
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Maximum entries to return (default: 100)
    #[serde(default = "default_sync_limit")]
    pub limit: usize,
}

// ─── State / Extraction / Ingest ────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SetStateParams {
    /// State key
    pub key: String,

    /// State value (JSON value)
    pub value: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetStateParams {
    /// State key
    pub key: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ExtractFactsParams {
    /// Text to extract facts from
    pub text: String,

    /// Source identifier for the extraction
    #[serde(default = "default_extraction_source")]
    pub source: String,
}

/// A single message in a conversation turn
#[derive(Debug, Clone, Deserialize, serde::Serialize, JsonSchema)]
pub(crate) struct Message {
    /// Role of the message sender (e.g., "user", "assistant", "system")
    #[allow(dead_code)]
    pub role: String,
    /// Content of the message
    pub content: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct IngestEventParams {
    /// Conversation identifier
    pub conversation_id: String,

    /// Turn identifier
    pub turn_id: String,

    /// Messages in the conversation turn
    pub messages: Vec<Message>,
}

// ─── Domain Management ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct RegisterDomainParams {
    /// Unique domain name (e.g. "finance", "code-review")
    pub name: String,

    /// Human-readable description of this domain
    #[serde(default)]
    pub description: Option<String>,

    /// GC stale-days threshold for memories in this domain (default: 90)
    #[serde(default)]
    pub gc_threshold_days: Option<u32>,

    /// Default retention policy for memories saved to this domain
    #[serde(default)]
    pub default_retention: Option<String>,

    /// Default path prefix for memories saved to this domain
    #[serde(default)]
    pub default_path_prefix: Option<String>,

    /// Arbitrary JSON metadata
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GetDomainParams {
    /// Domain name to retrieve
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ListDomainsParams {
    /// Placeholder (no filters currently needed)
    #[serde(default)]
    pub _placeholder: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct DeleteDomainParams {
    /// Domain name to delete
    pub name: String,
}

/// Build a MemoryEntry from a JSON fact value (shared by extract_facts and ingest_event).
pub(crate) fn fact_to_entry(
    fact: &serde_json::Value,
    source: &str,
    metadata: serde_json::Value,
) -> Option<MemoryEntry> {
    let text = fact["text"].as_str().unwrap_or("").to_string();
    if text.is_empty() {
        return None;
    }
    let topic = fact["topic"].as_str().unwrap_or("").to_string();
    let importance = fact["importance"].as_f64().unwrap_or(0.7).clamp(0.0, 1.0);
    let keywords: Vec<String> = fact["keywords"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let scope_raw = fact["scope"].as_str().unwrap_or("general");
    let scope = match scope_raw {
        "user" | "project" | "general" => scope_raw.to_string(),
        _ => "general".to_string(),
    };
    let summary = text.chars().take(100).collect::<String>();
    Some(MemoryEntry {
        id: uuid::Uuid::new_v4().to_string(),
        path: format!("/{}/{}", scope, topic.replace(' ', "_")),
        summary,
        text,
        importance,
        timestamp: Utc::now().to_rfc3339(),
        category: "fact".to_string(),
        topic,
        keywords,
        persons: vec![],
        entities: vec![],
        location: String::new(),
        source: source.to_string(),
        scope,
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata,
        vector: None,
        retention_policy: None,
        domain: None,
    })
}
