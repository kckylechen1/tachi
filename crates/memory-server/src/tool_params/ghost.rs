use super::*;

#[allow(dead_code)]
fn default_ghost_reflect_promote_rule() -> bool {
    true
}

fn default_access_level() -> String {
    "read".to_string()
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

// ─── Ghost Whispers ─────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GhostPublishParams {
    /// Topic to publish to (e.g. "build-status", "code-review")
    pub topic: String,
    /// Message payload (any JSON value)
    pub payload: serde_json::Value,
    /// Publisher agent identifier
    pub publisher: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GhostSubscribeParams {
    /// Unique agent identifier for cursor tracking
    pub agent_id: String,
    /// Topics to subscribe to and poll for new messages
    pub topics: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GhostAckParams {
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
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct GhostReflectParams {
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
pub(crate) struct GhostPromoteParams {
    /// Ghost message id to promote
    pub message_id: String,
    /// Optional memory path (default: /ghost/messages)
    #[serde(default)]
    pub path: Option<String>,
    /// Optional promoted importance
    #[serde(default)]
    pub importance: Option<f64>,
}

// ─── Dead Letter Queue ──────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct DlqListParams {
    /// Filter by status: "pending", "retrying", "resolved", "abandoned"
    #[serde(default)]
    pub status_filter: Option<String>,
    /// Max entries to return (default: 50)
    #[serde(default)]
    pub limit: Option<usize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct DlqRetryParams {
    /// ID of the dead letter entry to retry
    pub dead_letter_id: String,
}

// ─── Sandbox Access Rules ───────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SandboxSetRuleParams {
    /// Agent role (e.g. "code-review", "finance", "admin")
    pub agent_role: String,
    /// Path pattern to match (e.g. "/finance/*", "/project/secrets")
    pub path_pattern: String,
    /// Access level: "read", "write", or "deny"
    #[serde(default = "default_access_level")]
    pub access_level: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SandboxCheckParams {
    /// Agent role to check access for
    pub agent_role: String,
    /// Memory path to check
    pub path: String,
    /// Operation type: "read" or "write"
    #[serde(default = "default_sandbox_operation")]
    pub operation: String,
}

// ─── Sandbox Execution Policies ─────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SandboxSetPolicyParams {
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
pub(crate) struct SandboxGetPolicyParams {
    /// Capability ID to query
    pub capability_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SandboxListPoliciesParams {
    /// Only return enabled policies
    #[serde(default)]
    pub enabled_only: bool,
    /// Max rows returned
    #[serde(default = "default_sandbox_policy_limit")]
    pub limit: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SandboxExecAuditParams {
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
