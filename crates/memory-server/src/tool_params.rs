use super::*;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SaveMemoryParams {
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
}

fn default_auto_link() -> bool {
    true
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct RunSkillParams {
    /// ID of the skill capability to execute (e.g. "skill:code-review")
    pub skill_id: String,

    /// Arguments to inject into the skill's prompt template
    #[serde(default)]
    pub args: serde_json::Value,
}

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

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HybridWeightsParam {
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SearchMemoryParams {
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
}

impl SearchMemoryParams {
    /// Build SearchOptions from params, only differing by vec_available per DB.
    pub(super) fn to_search_options(&self, vec_available: bool) -> SearchOptions {
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
            ..Default::default()
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct FindSimilarMemoryParams {
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

fn default_find_similar_top_k() -> usize {
    5
}

/// Build a MemoryEntry from a JSON fact value (shared by extract_facts and ingest_event).
pub(super) fn fact_to_entry(
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
    })
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

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GetMemoryParams {
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
pub(super) struct ListMemoriesParams {
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

#[allow(dead_code)]
fn default_limit() -> usize {
    100
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SetStateParams {
    /// State key
    pub key: String,

    /// State value (JSON value)
    pub value: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GetStateParams {
    /// State key
    pub key: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ExtractFactsParams {
    /// Text to extract facts from
    pub text: String,

    /// Source identifier for the extraction
    #[serde(default = "default_extraction_source")]
    pub source: String,
}

#[allow(dead_code)]
fn default_extraction_source() -> String {
    "extraction".to_string()
}

/// A single message in a conversation turn
#[derive(Debug, Clone, Deserialize, serde::Serialize, JsonSchema)]
pub(super) struct Message {
    /// Role of the message sender (e.g., "user", "assistant", "system")
    #[allow(dead_code)]
    pub role: String,
    /// Content of the message
    pub content: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct IngestEventParams {
    /// Conversation identifier
    pub conversation_id: String,

    /// Turn identifier
    pub turn_id: String,

    /// Messages in the conversation turn
    pub messages: Vec<Message>,
}

#[allow(dead_code)]
fn default_hub_version() -> u32 {
    1
}

fn default_recall_candidate_multiplier() -> usize {
    3
}

fn default_capture_min_chars() -> usize {
    24
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct RecallContextParams {
    /// User or agent query that should be used to recall prior context
    pub query: String,

    /// Number of final results to keep after filtering / reranking
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Candidate expansion multiplier before reranking (default: 3)
    #[serde(default = "default_recall_candidate_multiplier")]
    pub candidate_multiplier: usize,

    /// Optional path prefix filter
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Topic names to exclude from the final context block
    #[serde(default)]
    pub exclude_topics: Vec<String>,

    /// Optional minimum score threshold after ranking
    #[serde(default)]
    pub min_score: Option<f64>,

    /// Optional agent role for sandbox filtering
    #[serde(default)]
    pub agent_role: Option<String>,

    /// Optional named project DB
    #[serde(default)]
    pub project: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct CaptureSessionParams {
    /// Conversation identifier
    pub conversation_id: String,

    /// Turn identifier
    pub turn_id: String,

    /// Canonical agent id for pathing / provenance
    pub agent_id: String,

    /// Messages in the session window
    pub messages: Vec<Message>,

    /// Optional base path for written memories
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Scope: "user" | "project" | "general"
    #[serde(default = "default_scope")]
    pub scope: String,

    /// Optional named project DB
    #[serde(default)]
    pub project: Option<String>,

    /// Minimum combined character count before capture runs
    #[serde(default = "default_capture_min_chars")]
    pub min_chars: usize,

    /// Force capture even when the window is short
    #[serde(default)]
    pub force: bool,
}

fn default_virtual_binding_priority() -> i32 {
    100
}

fn default_project_db_relpath() -> String {
    ".tachi/memory.db".to_string()
}

#[allow(dead_code)]
fn default_hub_review_status() -> String {
    "approved".to_string()
}

#[allow(dead_code)]
fn default_true() -> bool {
    true
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubRegisterParams {
    /// Unique capability ID, e.g. "skill:code-review", "mcp:github"
    pub id: String,
    /// Type: "skill" | "plugin" | "mcp"
    pub cap_type: String,
    /// Human-readable name
    pub name: String,
    /// Short description
    #[serde(default)]
    pub description: String,
    /// JSON string of capability definition (prompt template, manifest, config)
    #[serde(default)]
    pub definition: String,
    /// Version number
    #[serde(default = "default_hub_version")]
    pub version: u32,
    /// Target database scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    pub scope: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubDiscoverParams {
    /// Optional search query (searches name + description)
    #[serde(default)]
    pub query: Option<String>,
    /// Optional type filter: "skill" | "plugin" | "mcp"
    #[serde(default)]
    pub cap_type: Option<String>,
    /// Only return enabled capabilities (default: true)
    #[serde(default = "default_true")]
    pub enabled_only: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubGetParams {
    /// Capability ID to retrieve
    pub id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubFeedbackParams {
    /// Capability ID
    pub id: String,
    /// Whether the invocation was successful
    pub success: bool,
    /// Optional user rating (0.0 - 5.0)
    #[serde(default)]
    pub rating: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubCallParams {
    /// MCP server capability ID (e.g. "mcp:github")
    pub server_id: String,
    /// Tool name to call on the child MCP server
    pub tool_name: String,
    /// JSON arguments to pass to the tool
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct HubDisconnectParams {
    /// MCP server capability ID (e.g. "mcp:longbridge") or server name
    pub server_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct DeleteMemoryParams {
    /// Memory entry ID to delete
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ArchiveMemoryParams {
    /// Memory entry ID to archive
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubSetEnabledParams {
    /// Capability ID
    pub id: String,
    /// Whether to enable (true) or disable (false)
    pub enabled: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubReviewParams {
    /// Capability ID
    pub id: String,
    /// Governance review status: pending | approved | rejected
    #[serde(default = "default_hub_review_status")]
    pub review_status: String,
    /// Optional explicit enabled state override
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HubSetActiveVersionParams {
    /// Alias capability id (logical entrypoint)
    pub alias_id: String,
    /// Concrete capability id routed by this alias
    pub active_capability_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VirtualCapabilityRegisterParams {
    /// Virtual capability ID, e.g. "vc:web_search"
    pub id: String,
    /// Human-readable name
    pub name: String,
    /// Short description
    #[serde(default)]
    pub description: String,
    /// Contract / intent kind, e.g. "web_search", "docs_search"
    #[serde(default)]
    pub contract: String,
    /// Routing strategy. Current implementation uses "priority".
    #[serde(default)]
    pub routing_strategy: String,
    /// Agent-readable tags
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional JSON input schema for the logical capability
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// Target database scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    pub scope: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VirtualCapabilityBindParams {
    /// Virtual capability ID, e.g. "vc:web_search"
    pub vc_id: String,
    /// Concrete target capability ID, e.g. "mcp:exa"
    pub capability_id: String,
    /// Lower numbers win during deterministic routing.
    #[serde(default = "default_virtual_binding_priority")]
    pub priority: i32,
    /// Optional version pin. If set, target version must match during resolve.
    #[serde(default)]
    pub version_pin: Option<u32>,
    /// Whether this binding is active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Free-form binding metadata
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct VirtualCapabilityResolveParams {
    /// Virtual capability ID to resolve
    pub id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct InitProjectDbParams {
    /// Optional target repository root. Defaults to current git root.
    #[serde(default)]
    pub project_root: Option<String>,
    /// Relative DB path under the repo root.
    #[serde(default = "default_project_db_relpath")]
    pub db_relpath: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AddEdgeParams {
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

fn default_edge_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GetEdgesParams {
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

fn default_edge_direction() -> String {
    "both".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AuditLogParams {
    /// Maximum entries to return (default: 50)
    #[serde(default = "default_audit_limit")]
    pub limit: usize,
    /// Optional server_id filter
    #[serde(default)]
    pub server_filter: Option<String>,
}

#[allow(dead_code)]
fn default_audit_limit() -> usize {
    50
}

#[allow(dead_code)]
fn default_sync_limit() -> usize {
    100
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SyncMemoriesParams {
    /// Unique agent identifier for tracking known state
    pub agent_id: String,
    /// Optional path prefix to scope the sync (e.g. "/project")
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Maximum entries to return (default: 100)
    #[serde(default = "default_sync_limit")]
    pub limit: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GhostPublishParams {
    /// Topic to publish to (e.g. "build-status", "code-review")
    pub topic: String,
    /// Message payload (any JSON value)
    pub payload: serde_json::Value,
    /// Publisher agent identifier
    pub publisher: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GhostSubscribeParams {
    /// Unique agent identifier for cursor tracking
    pub agent_id: String,
    /// Topics to subscribe to and poll for new messages
    pub topics: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GhostAckParams {
    /// Unique agent identifier for cursor tracking
    pub agent_id: String,
    /// Topic to acknowledge
    pub topic: String,
    /// Optional explicit topic index to acknowledge up to
    #[serde(default)]
    pub index: Option<u64>,
    /// Optional message id to acknowledge up to
    #[serde(default)]
    pub message_id: Option<String>,
}

#[allow(dead_code)]
fn default_ghost_reflect_promote_rule() -> bool {
    true
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GhostReflectParams {
    /// Agent generating the reflection
    pub agent_id: String,
    /// Optional topic scope for this reflection
    #[serde(default)]
    pub topic: Option<String>,
    /// Reflection summary text
    pub summary: String,
    /// Optional structured metadata
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Whether to promote reflection into derived rules memory
    #[serde(default = "default_ghost_reflect_promote_rule")]
    pub promote_rule: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct GhostPromoteParams {
    /// Ghost message id to promote
    pub message_id: String,
    /// Optional memory path (default: /ghost/messages)
    #[serde(default)]
    pub path: Option<String>,
    /// Optional promoted importance
    #[serde(default)]
    pub importance: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct DlqListParams {
    /// Filter by status: "pending", "retrying", "resolved", "abandoned"
    #[serde(default)]
    pub status_filter: Option<String>,
    /// Max entries to return (default: 50)
    #[serde(default)]
    pub limit: Option<usize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct DlqRetryParams {
    /// ID of the dead letter entry to retry
    pub dead_letter_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxSetRuleParams {
    /// Agent role (e.g. "code-review", "finance", "admin")
    pub agent_role: String,
    /// Path pattern to match (e.g. "/finance/*", "/project/secrets")
    pub path_pattern: String,
    /// Access level: "read", "write", or "deny"
    #[serde(default = "default_access_level")]
    pub access_level: String,
}

fn default_access_level() -> String {
    "read".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxCheckParams {
    /// Agent role to check access for
    pub agent_role: String,
    /// Memory path to check
    pub path: String,
    /// Operation type: "read" or "write"
    #[serde(default = "default_sandbox_operation")]
    pub operation: String,
}

fn default_sandbox_operation() -> String {
    "read".to_string()
}

#[allow(dead_code)]
fn default_runtime_type() -> String {
    "process".to_string()
}

#[allow(dead_code)]
fn default_sandbox_startup_ms() -> u64 {
    30_000
}

#[allow(dead_code)]
fn default_sandbox_tool_ms() -> u64 {
    30_000
}

#[allow(dead_code)]
fn default_sandbox_max_concurrency() -> u32 {
    1
}

#[allow(dead_code)]
fn default_true_bool() -> bool {
    true
}

#[allow(dead_code)]
fn default_sandbox_policy_limit() -> usize {
    100
}

#[allow(dead_code)]
fn default_sandbox_exec_audit_limit() -> usize {
    100
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxSetPolicyParams {
    /// Capability ID (typically MCP capability id, e.g. "mcp:exa")
    pub capability_id: String,
    /// Runtime type: "process" | "wasm"
    #[serde(default = "default_runtime_type")]
    pub runtime_type: String,
    /// Environment variable allowlist. Empty means keep existing behavior.
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    /// Allowed read roots for filesystem access (advisory + cwd guard).
    #[serde(default)]
    pub fs_read_roots: Vec<String>,
    /// Allowed write roots for filesystem access (reserved for executors).
    #[serde(default)]
    pub fs_write_roots: Vec<String>,
    /// Allowed working-directory roots for process startup.
    #[serde(default)]
    pub cwd_roots: Vec<String>,
    /// Startup timeout cap in milliseconds.
    #[serde(default = "default_sandbox_startup_ms")]
    pub max_startup_ms: u64,
    /// Tool call timeout cap in milliseconds.
    #[serde(default = "default_sandbox_tool_ms")]
    pub max_tool_ms: u64,
    /// Max concurrency cap for the capability.
    #[serde(default = "default_sandbox_max_concurrency")]
    pub max_concurrency: u32,
    /// Whether this policy is enabled.
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxGetPolicyParams {
    /// Capability ID to query
    pub capability_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxListPoliciesParams {
    /// Only return enabled policies
    #[serde(default)]
    pub enabled_only: bool,
    /// Max rows returned
    #[serde(default = "default_sandbox_policy_limit")]
    pub limit: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SandboxExecAuditParams {
    /// Optional capability filter (e.g. "mcp:exa")
    #[serde(default)]
    pub capability_id: Option<String>,
    /// Optional stage filter (e.g. "preflight", "startup", "tool_call")
    #[serde(default)]
    pub stage: Option<String>,
    /// Optional decision filter (e.g. "allowed", "denied", "timeout", "failed")
    #[serde(default)]
    pub decision: Option<String>,
    /// Max rows returned.
    #[serde(default = "default_sandbox_exec_audit_limit")]
    pub limit: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ChainStep {
    /// Skill capability ID (e.g. "skill:summarize")
    pub skill_id: String,
    /// Extra arguments to merge with piped input
    #[serde(default)]
    pub extra_args: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ChainSkillsParams {
    /// Ordered list of skill steps to execute
    pub steps: Vec<ChainStep>,
    /// Input for the first step
    pub initial_input: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ExportSkillsParams {
    /// Target agent format: "claude", "openclaw", "cursor", "generic"
    /// Defaults to "claude" if omitted.
    #[serde(default = "default_export_agent")]
    pub agent: String,

    /// Only export skills matching these IDs (e.g. ["skill:code-review"]).
    /// If omitted, exports all enabled skills matching the visibility filter.
    #[serde(default)]
    pub skill_ids: Option<Vec<String>>,

    /// Visibility filter: "listed", "discoverable", "all" (default: "listed")
    #[serde(default = "default_export_visibility")]
    pub visibility: String,

    /// Override output directory. If omitted, uses agent-specific defaults:
    /// - claude: ~/.tachi/skills/ (with symlinks to ~/.claude/skills/)
    /// - openclaw: ~/.openclaw/plugins/
    /// - cursor: .cursor/rules/ (relative to cwd)
    /// - generic: ./exported-skills/
    #[serde(default)]
    pub output_dir: Option<String>,

    /// If true, remove skills from the target directory that are not in the export set (default: false)
    #[serde(default)]
    pub clean: bool,
}

fn default_export_agent() -> String {
    "claude".to_string()
}

fn default_export_visibility() -> String {
    "listed".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AgentRegisterParams {
    /// Unique agent identifier (e.g. "claude-code", "openclaw", "cursor", "codex")
    pub agent_id: String,

    /// Human-readable display name
    #[serde(default)]
    pub display_name: Option<String>,

    /// Agent capabilities / feature flags (e.g. ["code-gen", "file-edit", "web-search"])
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Optional tool allowlist — if set, only these tools will be returned in list_tools.
    /// Supports glob patterns like "hub_*", "save_memory", "tachi_skill_*".
    #[serde(default)]
    pub tool_filter: Option<Vec<String>>,

    /// Optional per-agent rate limit override (requests per minute, 0 = use server default)
    #[serde(default)]
    pub rate_limit_rpm: Option<u64>,

    /// Optional per-agent burst limit override (0 = use server default)
    #[serde(default)]
    pub rate_limit_burst: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AgentWhoamiParams {
    /// No parameters needed — returns the current agent profile for this session.
    #[serde(default)]
    pub _placeholder: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HandoffLeaveParams {
    /// Summary of what was accomplished in this session
    pub summary: String,

    /// List of incomplete tasks / next steps for the receiving agent
    #[serde(default)]
    pub next_steps: Vec<String>,

    /// Optional target agent ID (e.g. "claude-code", "cursor"). If omitted, any agent can pick up.
    #[serde(default)]
    pub target_agent: Option<String>,

    /// Optional context (file paths, error messages, etc.)
    #[serde(default)]
    pub context: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct HandoffCheckParams {
    /// Agent ID checking for handoff memos. If omitted, returns all pending memos.
    #[serde(default)]
    pub agent_id: Option<String>,

    /// If true, mark retrieved memos as acknowledged (default: true)
    #[serde(default = "default_true")]
    pub acknowledge: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SkillEvolveParams {
    /// Skill capability ID to evolve (e.g. "skill:code-review")
    pub skill_id: String,

    /// Optional user feedback about what to improve in the skill
    #[serde(default)]
    pub feedback: Option<String>,

    /// If true, automatically activate the new version (default: false)
    #[serde(default)]
    pub auto_activate: bool,

    /// If true, perform a dry-run — return the proposed improved prompt without persisting (default: false)
    #[serde(default)]
    pub dry_run: bool,
}

#[allow(dead_code)]
fn default_foundry_evidence_weight() -> f64 {
    1.0
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AgentEvolutionDocumentParams {
    /// Document kind: identity | agents | latest_truths | routing_policy | tool_policy | memory_policy | other
    pub kind: String,

    /// Optional source path for provenance / later projection
    #[serde(default)]
    pub path: Option<String>,

    /// Current document content
    pub content: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct AgentEvolutionEvidenceParams {
    /// Evidence kind: memory | reflection | tooluse | eval | ghost | session_outcome | skill_telemetry | profile_snapshot | proposal | other
    pub kind: String,

    /// Optional short evidence title
    #[serde(default)]
    pub title: Option<String>,

    /// Raw evidence text or summary
    pub content: String,

    /// Optional evidence identifier for traceability
    #[serde(default)]
    pub source_ref: Option<String>,

    /// Optional filesystem or logical path
    #[serde(default)]
    pub path: Option<String>,

    /// Relative evidence weight (default: 1.0)
    #[serde(default = "default_foundry_evidence_weight")]
    pub weight: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct SynthesizeAgentEvolutionParams {
    /// Canonical target agent id
    pub agent_id: String,

    /// Optional display name for synthesis context
    #[serde(default)]
    pub display_name: Option<String>,

    /// Current profile documents that should inform the proposal
    #[serde(default)]
    pub documents: Vec<AgentEvolutionDocumentParams>,

    /// Supporting evidence items
    #[serde(default)]
    pub evidence: Vec<AgentEvolutionEvidenceParams>,

    /// Optional operator goals or focus areas
    #[serde(default)]
    pub goals: Vec<String>,

    /// If true, do not call the LLM. Return the normalized job and request payload only.
    #[serde(default)]
    pub dry_run: bool,
}

// ─── Pack System Params ─────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PackListParams {
    /// If true, only return enabled packs (default: false)
    #[serde(default)]
    pub enabled_only: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PackGetParams {
    /// Pack identifier, e.g. "garrytan/gstack" or "obra/superpowers"
    pub id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PackRegisterParams {
    /// Pack identifier, e.g. "garrytan/gstack"
    pub id: String,

    /// Display name
    #[serde(default)]
    pub name: Option<String>,

    /// Source URI: "github:owner/repo", "local:/path/to/pack"
    #[serde(default)]
    pub source: Option<String>,

    /// Version string (git tag, commit sha, or semver)
    #[serde(default)]
    pub version: Option<String>,

    /// Short description
    #[serde(default)]
    pub description: Option<String>,

    /// Local filesystem path where the pack is stored
    #[serde(default)]
    pub local_path: Option<String>,

    /// Extra metadata (JSON object)
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PackRemoveParams {
    /// Pack identifier to remove
    pub id: String,

    /// If true (default), also delete projected files from agent directories
    #[serde(default)]
    pub clean_files: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct PackProjectParams {
    /// Pack identifier to project
    pub pack_id: String,

    /// List of agent kinds to project to: "claude", "codex", "cursor", "gemini", "openclaw", "opencode", "antigravity", "trae", "kiro", "generic"
    pub agents: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct ProjectionListParams {
    /// Filter by agent kind (optional)
    #[serde(default)]
    pub agent: Option<String>,

    /// Filter by pack ID (optional)
    #[serde(default)]
    pub pack_id: Option<String>,
}
