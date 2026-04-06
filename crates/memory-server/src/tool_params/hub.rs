use super::*;

#[allow(dead_code)]
fn default_hub_version() -> u32 {
    1
}

fn default_scope() -> String {
    "project".to_string()
}

#[allow(dead_code)]
fn default_hub_review_status() -> String {
    "approved".to_string()
}

#[allow(dead_code)]
fn default_true() -> bool {
    true
}

fn default_export_agent() -> String {
    "claude".to_string()
}

fn default_export_visibility() -> String {
    "listed".to_string()
}

fn default_virtual_binding_priority() -> i32 {
    100
}

#[allow(dead_code)]
fn default_audit_limit() -> usize {
    50
}

// ─── Registration / Discovery ───────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct RunSkillParams {
    /// ID of the skill capability to execute (e.g. "skill:code-review")
    pub skill_id: String,

    /// Arguments to inject into the skill's prompt template
    #[serde(default)]
    pub args: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct DistillTrajectoryParams {
    /// Natural-language task description
    pub task_description: String,

    /// Key execution steps, failures, and recoveries
    pub execution_trace: Vec<serde_json::Value>,

    /// Final outcome payload, e.g. success flag, score, notes
    pub final_outcome: serde_json::Value,

    /// Source agent identifier
    pub agent_id: String,

    /// Memory / skill path, e.g. /skills/hyperion/factor-evolution
    pub skill_path: String,

    /// Optional distilled skill capability id. Defaults to one derived from skill_path.
    #[serde(default)]
    pub skill_id: Option<String>,

    /// Optional base importance for the permanent snapshot memory
    #[serde(default)]
    pub importance: Option<f64>,

    /// Optional domain. Defaults to TACHI_DOMAIN when present.
    #[serde(default)]
    pub domain: Option<String>,

    /// Optional named project DB target
    #[serde(default)]
    pub project: Option<String>,

    /// Target database scope for snapshot writes
    #[serde(default = "default_scope")]
    pub scope: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HubRegisterParams {
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
pub(crate) struct HubDiscoverParams {
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
pub(crate) struct HubGetParams {
    /// Capability ID to retrieve
    pub id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HubFeedbackParams {
    /// Capability ID
    pub id: String,
    /// Whether the invocation was successful
    pub success: bool,
    /// Optional user rating (0.0 - 5.0)
    #[serde(default)]
    pub rating: Option<f64>,
}

// ─── Call / Proxy ───────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HubCallParams {
    /// MCP server capability ID (e.g. "mcp:github")
    pub server_id: String,
    /// Tool name to call on the child MCP server
    pub tool_name: String,
    /// JSON arguments to pass to the tool
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct HubDisconnectParams {
    /// MCP server capability ID (e.g. "mcp:longbridge") or server name
    pub server_id: String,
}

// ─── Review / Governance ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HubSetEnabledParams {
    /// Capability ID
    pub id: String,
    /// Whether to enable (true) or disable (false)
    pub enabled: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct HubReviewParams {
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
pub(crate) struct HubSetActiveVersionParams {
    /// Alias capability id (logical entrypoint)
    pub alias_id: String,
    /// Concrete capability id routed by this alias
    pub active_capability_id: String,
}

// ─── Virtual Capability ─────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct VirtualCapabilityRegisterParams {
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
pub(crate) struct VirtualCapabilityBindParams {
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
pub(crate) struct VirtualCapabilityResolveParams {
    /// Virtual capability ID to resolve
    pub id: String,
}

// ─── Export / Evolve / Chain ────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ExportSkillsParams {
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

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SkillEvolveParams {
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ChainStep {
    /// Skill capability ID (e.g. "skill:summarize")
    pub skill_id: String,
    /// Extra arguments to merge with piped input
    #[serde(default)]
    pub extra_args: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ChainSkillsParams {
    /// Ordered list of skill steps to execute
    pub steps: Vec<ChainStep>,
    /// Input for the first step
    pub initial_input: String,
}

// ─── Audit Log ──────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct AuditLogParams {
    /// Maximum entries to return (default: 50)
    #[serde(default = "default_audit_limit")]
    pub limit: usize,
    /// Optional server_id filter
    #[serde(default)]
    pub server_filter: Option<String>,
}
