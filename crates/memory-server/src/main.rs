// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod kanban;
mod llm;
mod mcp_connection;
mod mcp_proxy;
mod prompts;

use crate::kanban::{
    card_from_agent, card_matches_inbox, card_metadata_str, card_priority, card_status, card_to_agent,
    card_type, enrich_kanban_card_classification, kanban_priority_importance, kanban_priority_rank,
    normalize_agent_id, normalize_card_priority, normalize_card_status, normalize_card_type,
    CheckInboxParams, KANBAN_CATEGORY, KANBAN_PATH_PREFIX, PostCardParams, UpdateCardParams,
};
use crate::mcp_proxy::{
    append_warning, clear_mcp_discovery_metadata, filter_mcp_tools_by_permissions,
    resolve_mcp_tool_exposure, set_mcp_discovery_failure, set_mcp_discovery_success,
    McpToolExposureMode,
};

use chrono::Utc;
use clap::Parser;
use memory_core::{HubCapability, HybridWeights, MemoryEntry, MemoryStore, SearchOptions};
use rmcp::{
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    schemars::JsonSchema,
    tool, tool_router,
    transport::StreamableHttpClientTransport,
    ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};
use tokio::io::{stdin, stdout};

// ─── CLI Arguments ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "tachi", version, about = "Tachi — memory + Hub MCP server")]
struct Cli {
    /// Run as HTTP daemon instead of stdio transport
    #[arg(long)]
    daemon: bool,

    /// Port for HTTP daemon (default: 6919)
    #[arg(long, default_value_t = 6919)]
    port: u16,

    /// Override global memory DB path (equivalent to MEMORY_DB_PATH)
    #[arg(long, value_name = "PATH")]
    global_db: Option<PathBuf>,

    /// Override project memory DB path
    #[arg(long, value_name = "PATH")]
    project_db: Option<PathBuf>,

    /// Disable project DB entirely (force single-DB mode)
    #[arg(long)]
    no_project_db: bool,

    /// Enable/disable background database GC (overrides MEMORY_GC_ENABLED)
    #[arg(long)]
    gc_enabled: Option<bool>,

    /// Delay before first background GC run in seconds (overrides MEMORY_GC_INITIAL_DELAY_SECS)
    #[arg(long)]
    gc_initial_delay_secs: Option<u64>,

    /// Interval between background GC runs in seconds (overrides MEMORY_GC_INTERVAL_SECS)
    #[arg(long)]
    gc_interval_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbScope {
    Global,
    Project,
}

impl DbScope {
    fn as_str(&self) -> &'static str {
        match self {
            DbScope::Global => "global",
            DbScope::Project => "project",
        }
    }
}

/// Build a slim JSON representation of a MemoryEntry for MCP output.
/// Strips internal fields (access_count, last_access, revision, source, vector)
/// and omits empty strings/arrays to minimize token usage.
fn slim_entry(e: &MemoryEntry, db: DbScope) -> serde_json::Value {
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
    if e.archived {
        obj.insert("archived".into(), json!(true));
    }
    // Only include metadata if non-empty object
    if let serde_json::Value::Object(ref m) = e.metadata {
        if !m.is_empty() {
            obj.insert("metadata".into(), json!(m));
        }
    }
    serde_json::Value::Object(obj)
}

/// Build a slim search result: entry fields + single relevance score.
fn slim_search_result(result: &memory_core::SearchResult, db: DbScope) -> serde_json::Value {
    let mut obj = match slim_entry(&result.entry, db) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    // Round to 3 decimal places
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

/// Build a slim L0 rule entry.
fn slim_l0_rule(rule: &MemoryEntry, db: DbScope) -> serde_json::Value {
    let mut obj = match slim_entry(rule, db) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("l0_rule".into(), json!(true));
    serde_json::Value::Object(obj)
}

// ─── Server State ─────────────────────────────────────────────────────────────

/// TTL for cached tool results (Phantom Tools)
const TOOL_CACHE_TTL: Duration = Duration::from_secs(30);
const DEFAULT_MCP_DISCOVERY_TIMEOUT_MS: u64 = 10_000;

/// Tools whose results can be cached (read-only, no side effects)
const CACHEABLE_TOOLS: &[&str] = &[
    "search_memory",
    "find_similar_memory",
    "get_memory",
    "list_memories",
    "memory_stats",
    "get_state",
    "hub_discover",
    "hub_get",
    "hub_stats",
    "get_pipeline_status",
];

/// Tools that invalidate the cache (write operations)
const CACHE_INVALIDATING_TOOLS: &[&str] = &[
    "save_memory",
    "extract_facts",
    "ingest_event",
    "set_state",
    "hub_register",
    "hub_feedback",
];

struct CachedResult {
    result: rmcp::model::CallToolResult,
    created_at: Instant,
}

impl Clone for CachedResult {
    fn clone(&self) -> Self {
        Self {
            result: self.result.clone(),
            created_at: self.created_at,
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
struct MemoryServer {
    global_store: Arc<StdMutex<MemoryStore>>,
    project_store: Option<Arc<StdMutex<MemoryStore>>>,
    global_db_path: Arc<PathBuf>,
    project_db_path: Option<Arc<PathBuf>>,
    global_vec_available: bool,
    project_vec_available: bool,
    llm: Arc<llm::LlmClient>,
    pipeline_enabled: bool,
    /// Cached proxy tools from registered MCP servers: server_id → Vec<Tool>
    proxy_tools: Arc<StdMutex<HashMap<String, Vec<rmcp::model::Tool>>>>,
    skill_tools: Arc<StdMutex<HashMap<String, String>>>,
    skill_tool_defs: Arc<StdMutex<HashMap<String, rmcp::model::Tool>>>,
    pool: Arc<McpClientPool>,
    tool_router: ToolRouter<Self>,
    // ─── Phantom Tools (result caching) ──────────────────────────────────────
    tool_cache: Arc<StdMutex<HashMap<String, CachedResult>>>,
    cache_hits: Arc<std::sync::atomic::AtomicU64>,
    cache_misses: Arc<std::sync::atomic::AtomicU64>,
    // ─── Ghost Whispers (pub/sub) ───────────────────────────────────────────
    pubsub: Arc<StdMutex<PubSubState>>,
    // ─── Dead Letter Queue (failed tool call auto-retry) ─────────────────
    dead_letters: Arc<StdMutex<VecDeque<DeadLetter>>>,
    mcp_discovery_timeout: Duration,
    mcp_tool_exposure_mode: McpToolExposureMode,
}

// ─── MCP Client Connection Pool ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

struct ChildConnection {
    /// The running MCP client service — we call peer() on this
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    last_used: Instant,
}

struct McpClientPool {
    /// Active connections: server_name → connection
    connections: std::sync::Mutex<HashMap<String, ChildConnection>>,
    /// Circuit breaker state per server
    circuits: std::sync::Mutex<HashMap<String, (CircuitState, u32)>>,
    /// Per-child concurrency semaphores: (semaphore, configured max_concurrency)
    semaphores: std::sync::Mutex<HashMap<String, (Arc<tokio::sync::Semaphore>, usize)>>,
    /// Per-child connecting locks to prevent TOCTOU race
    connecting_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Idle TTL before auto-disconnect
    idle_ttl: Duration,
}

impl McpClientPool {
    fn new() -> Self {
        Self {
            connections: std::sync::Mutex::new(HashMap::new()),
            circuits: std::sync::Mutex::new(HashMap::new()),
            semaphores: std::sync::Mutex::new(HashMap::new()),
            connecting_locks: std::sync::Mutex::new(HashMap::new()),
            idle_ttl: Duration::from_secs(300),
        }
    }
}

impl MemoryServer {
    fn new(
        global_db_path: PathBuf,
        project_db_path: Option<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Open stores once at startup (init_schema runs here, not per-request)
        let global_store = MemoryStore::open(global_db_path.to_str().unwrap())?;
        let global_vec_available = global_store.vec_available;

        let (project_store, project_db_path, project_vec_available) =
            if let Some(ref p) = project_db_path {
                let store = MemoryStore::open(p.to_str().unwrap())?;
                let v = store.vec_available;
                (
                    Some(Arc::new(StdMutex::new(store))),
                    Some(Arc::new(p.clone())),
                    v,
                )
            } else {
                (None, None, false)
            };

        let llm = Arc::new(llm::LlmClient::new()?);
        let pipeline_enabled = std::env::var("ENABLE_PIPELINE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let mcp_discovery_timeout_ms = match parse_env_u64("MCP_DISCOVERY_TIMEOUT_MS") {
            Some(0) => {
                eprintln!("MCP_DISCOVERY_TIMEOUT_MS must be >= 1; using 1ms");
                1
            }
            Some(value) => value,
            None => DEFAULT_MCP_DISCOVERY_TIMEOUT_MS,
        };
        let mcp_tool_exposure_mode = std::env::var("MCP_TOOL_EXPOSURE_MODE")
            .ok()
            .and_then(|raw| match McpToolExposureMode::from_str(&raw) {
                Some(mode) => Some(mode),
                None => {
                    eprintln!(
                        "Ignoring invalid MCP_TOOL_EXPOSURE_MODE value '{}' (expected flatten|gateway)",
                        raw
                    );
                    None
                }
            })
            .unwrap_or(McpToolExposureMode::Flatten);
        Ok(Self {
            global_store: Arc::new(StdMutex::new(global_store)),
            project_store,
            global_db_path: Arc::new(global_db_path),
            project_db_path,
            global_vec_available,
            project_vec_available,
            llm,
            pipeline_enabled,
            proxy_tools: Arc::new(StdMutex::new(HashMap::new())),
            skill_tools: Arc::new(StdMutex::new(HashMap::new())),
            skill_tool_defs: Arc::new(StdMutex::new(HashMap::new())),
            pool: Arc::new(McpClientPool::new()),
            tool_router: Self::tool_router(),
            tool_cache: Arc::new(StdMutex::new(HashMap::new())),
            cache_hits: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cache_misses: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pubsub: Arc::new(StdMutex::new(PubSubState::new())),
            dead_letters: Arc::new(StdMutex::new(VecDeque::new())),
            mcp_discovery_timeout: Duration::from_millis(mcp_discovery_timeout_ms),
            mcp_tool_exposure_mode,
        })
    }

    fn with_global_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut store = self.global_store.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut store)
    }

    fn with_project_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let store_arc = self
            .project_store
            .as_ref()
            .ok_or_else(|| "No project database available (not in a git repository)".to_string())?;
        let mut store = store_arc.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut store)
    }

    fn with_store_for_scope<T>(
        &self,
        scope: DbScope,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        match scope {
            DbScope::Global => self.with_global_store(f),
            DbScope::Project => self.with_project_store(f),
        }
    }

    /// Resolve which DB to write to based on the requested scope string.
    /// "global" → Global. Anything else → Project if available, else Global with warning.
    fn resolve_write_scope(&self, requested: &str) -> (DbScope, Option<String>) {
        if requested == "global" {
            (DbScope::Global, None)
        } else if self.project_db_path.is_some() {
            (DbScope::Project, None)
        } else {
            (
                DbScope::Global,
                Some("No project DB available; saved to global".to_string()),
            )
        }
    }

    fn get_capability(&self, cap_id: &str) -> Result<HubCapability, rmcp::ErrorData> {
        let mut found = None;
        if self.project_db_path.is_some() {
            found = self
                .with_project_store(|store| {
                    store
                        .hub_get(cap_id)
                        .map_err(|e| format!("hub get project: {e}"))
                })
                .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
        }
        if found.is_none() {
            found = self
                .with_global_store(|store| {
                    store
                        .hub_get(cap_id)
                        .map_err(|e| format!("hub get global: {e}"))
                })
                .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
        }
        found.ok_or_else(|| {
            rmcp::ErrorData::invalid_params(format!("Capability '{cap_id}' not found"), None)
        })
    }

    fn proxy_tool_exposure_mode_for_server(
        &self,
        server_name: &str,
    ) -> Result<McpToolExposureMode, rmcp::ErrorData> {
        let cap_id = format!("mcp:{server_name}");
        let cap = self.get_capability(&cap_id)?;
        let def: serde_json::Value = serde_json::from_str(&cap.definition).map_err(|e| {
            rmcp::ErrorData::internal_error(
                format!("bad definition for capability '{cap_id}': {e}"),
                None,
            )
        })?;
        Ok(resolve_mcp_tool_exposure(&def, self.mcp_tool_exposure_mode))
    }

    fn register_skill_tool(&self, cap: &HubCapability) -> Result<String, String> {
        let (tool_name, tool) = build_skill_tool_from_cap(cap)?;
        self.skill_tools
            .lock()
            .map_err(|e| e.to_string())?
            .insert(tool_name.clone(), cap.id.clone());
        self.skill_tool_defs
            .lock()
            .map_err(|e| e.to_string())?
            .insert(tool_name.clone(), tool);
        Ok(tool_name)
    }

    async fn call_skill_tool(
        &self,
        tool_name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let skill_id = self
            .skill_tools
            .lock()
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            .get(tool_name)
            .cloned()
            .ok_or_else(|| {
                rmcp::ErrorData::invalid_params(
                    format!("Skill tool '{}' not found", tool_name),
                    None,
                )
            })?;

        let cap = self.get_capability(&skill_id)?;
        let def: Value = serde_json::from_str(&cap.definition).map_err(|e| {
            rmcp::ErrorData::invalid_params(format!("Invalid skill definition JSON: {e}"), None)
        })?;

        let args = arguments.unwrap_or_default();
        let args_value = Value::Object(args.clone());
        let args_json = serde_json::to_string_pretty(&args_value)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("serialize args: {e}"), None))?;

        let mut prompt = def
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| def.get("template").and_then(|v| v.as_str()))
            .unwrap_or("{{args_json}}")
            .to_string();
        prompt = prompt.replace("{{args_json}}", &args_json);
        prompt = prompt.replace("{{args}}", &args_json);
        for (k, v) in &args {
            let key = format!("{{{{{k}}}}}");
            prompt = prompt.replace(&key, &value_to_template_text(v));
        }
        if prompt.contains("{{input}}") {
            let input = args
                .get("input")
                .map(value_to_template_text)
                .unwrap_or_else(|| args_json.clone());
            prompt = prompt.replace("{{input}}", &input);
        }

        let output = if let Some(mock_response) = def.get("mock_response").and_then(|v| v.as_str())
        {
            mock_response.to_string()
        } else {
            let system = def
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("You are executing a reusable skill. Follow the instruction and produce the result.");
            let model = def.get("model").and_then(|v| v.as_str());
            let temperature = def
                .get("temperature")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.2) as f32;
            let max_tokens = def
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(1200) as u32;
            self.llm
                .call_llm(system, &prompt, model, temperature, max_tokens)
                .await
                .map_err(|e| {
                    rmcp::ErrorData::internal_error(format!("skill execution failed: {e}"), None)
                })?
        };

        make_text_tool_result(&json!({
            "skill_id": skill_id,
            "tool_name": tool_name,
            "output": output
        }))
    }

    /// Shared retry dispatch for DLQ entries.
    /// Handles skill tools and proxy tools. Native tools are excluded from retry
    /// since they are synchronous and should be retried by the caller directly.
    async fn retry_dispatch(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let args_obj = arguments.map(|m| m.into_iter().collect::<rmcp::model::JsonObject>());

        // 1. Skill tool?
        if self
            .skill_tools
            .lock()
            .map(|s| s.contains_key(tool_name))
            .unwrap_or(false)
        {
            return self.call_skill_tool(tool_name, args_obj).await;
        }

        // 2. Proxy tool? (format: "server__tool")
        if let Some((server_name, remote_tool)) = tool_name.split_once("__") {
            let exposure_mode = self.proxy_tool_exposure_mode_for_server(server_name)?;
            if exposure_mode == McpToolExposureMode::Gateway {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Direct proxy tool '{}' is disabled by tool_exposure=gateway for '{}'. Retry via hub_call.",
                        tool_name, server_name
                    ),
                    None,
                ));
            }
            return self
                .proxy_call_internal(server_name, remote_tool, args_obj)
                .await;
        }

        // 3. Native tool — cannot retry via DLQ (needs RequestContext).
        //    These are marked abandoned; the client should retry the MCP call directly.
        Err(rmcp::ErrorData::invalid_params(
            format!(
                "Native tool '{}' cannot be retried via DLQ — retry the MCP call directly",
                tool_name
            ),
            None,
        ))
    }
}

fn is_active_global_rule(entry: &MemoryEntry) -> bool {
    entry
        .metadata
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("DRAFT")
        == "ACTIVE"
}

fn find_git_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Check if a command is in the trusted allowlist for MCP server spawning.
/// Trusted: common package runners, interpreters, and brew-installed binaries.
fn is_trusted_command(cmd: &str) -> bool {
    let basename = std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);

    const TRUSTED_BASENAMES: &[&str] = &[
        "npx", "node", "bun", "deno", "python3", "python", "uv", "cargo", "rustup", "docker",
        "podman", "tachi",
    ];

    if TRUSTED_BASENAMES.contains(&basename) {
        return true;
    }

    // Allow absolute paths under Homebrew, nvm, cargo, common bin dirs
    const TRUSTED_PREFIXES: &[&str] = &["/opt/homebrew/", "/usr/local/bin/", "/usr/bin/", "/bin/"];

    for prefix in TRUSTED_PREFIXES {
        if cmd.starts_with(prefix) {
            return true;
        }
    }

    // Allow paths under user's home .cargo/bin, .local/bin, .nvm
    if let Ok(home) = std::env::var("HOME") {
        let home_prefixes = [
            format!("{}/.cargo/bin/", home),
            format!("{}/.local/bin/", home),
            format!("{}/.nvm/", home),
            format!("{}/.bun/bin/", home),
        ];
        for prefix in &home_prefixes {
            if cmd.starts_with(prefix.as_str()) {
                return true;
            }
        }
    }

    false
}

fn sanitize_skill_tool_name(skill_id: &str) -> Option<String> {
    let raw = skill_id.strip_prefix("skill:")?;
    let mut output = String::from("tachi_skill_");
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            output.push(c.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    Some(output.trim_end_matches('_').to_string())
}

fn value_to_template_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        v.to_string()
    }
}

fn make_text_tool_result(payload: &Value) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let text = serde_json::to_string(payload)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("serialize response: {e}"), None))?;
    serde_json::from_value(json!({
        "content": [{"type": "text", "text": text}],
        "isError": false
    }))
    .map_err(|e| rmcp::ErrorData::internal_error(format!("build MCP response: {e}"), None))
}

fn build_skill_tool_from_cap(cap: &HubCapability) -> Result<(String, rmcp::model::Tool), String> {
    let tool_name = sanitize_skill_tool_name(&cap.id)
        .ok_or_else(|| format!("Invalid skill id '{}'", cap.id))?;
    let def: Value = serde_json::from_str(&cap.definition)
        .map_err(|e| format!("Invalid skill definition JSON: {e}"))?;
    let input_schema = def
        .get("inputSchema")
        .cloned()
        .or_else(|| def.get("input_schema").cloned())
        .unwrap_or(json!({
            "type": "object",
            "properties": {
                "input": {"type": "string", "description": "Primary input for this skill"},
                "context": {"type": "string", "description": "Optional context"}
            },
            "additionalProperties": true
        }));
    let description = if cap.description.is_empty() {
        format!("Run skill {}", cap.name)
    } else {
        cap.description.clone()
    };
    let tool = serde_json::from_value::<rmcp::model::Tool>(json!({
        "name": tool_name,
        "description": description,
        "inputSchema": input_schema
    }))
    .map_err(|e| format!("Build skill tool failed: {e}"))?;
    Ok((tool_name, tool))
}

// ─── Tool Parameter Types ───────────────────────────────────────────────────────
//
// Note: dead_code warnings are expected here because the #[tool] macro
// generates code that uses these types through macro expansion.

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SaveMemoryParams {
    /// Full text content of the memory
    text: String,

    /// Short summary (≤100 chars)
    #[serde(default)]
    summary: String,

    /// Hierarchical path, e.g. "/openclaw/agent-main"
    #[serde(default = "default_path")]
    path: String,

    /// 0.0–1.0 importance score
    #[serde(default = "default_importance")]
    importance: f64,

    /// Category: "fact" | "decision" | "experience" | "preference" | "entity" | "other"
    #[serde(default = "default_category")]
    category: String,

    /// Topic / subject area
    #[serde(default)]
    topic: String,

    /// Keyword tags
    #[serde(default)]
    keywords: Vec<String>,

    /// Person names mentioned
    #[serde(default)]
    persons: Vec<String>,

    /// Entity names mentioned
    #[serde(default)]
    entities: Vec<String>,

    /// Physical or logical location
    #[serde(default)]
    location: String,

    /// Scope: "user" | "project" | "general"
    #[serde(default = "default_scope")]
    scope: String,

    /// Optional embedding vector (if provided, skip embedding generation)
    #[serde(default)]
    vector: Option<Vec<f32>>,

    /// Optional entry ID (for updates)
    #[serde(default)]
    id: Option<String>,

    /// Bypass noise filter and force-save text
    #[serde(default)]
    force: bool,

    /// Auto-link: create graph edges to memories sharing the same entities (default: true)
    #[serde(default = "default_auto_link")]
    auto_link: bool,
}

fn default_auto_link() -> bool {
    true
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct RunSkillParams {
    /// ID of the skill capability to execute (e.g. "skill:code-review")
    skill_id: String,

    /// Arguments to inject into the skill's prompt template
    #[serde(default)]
    args: serde_json::Value,
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
struct HybridWeightsParam {
    /// Semantic (vector) weight (default: 0.4)
    #[serde(default = "default_weight_semantic")]
    semantic: f64,
    /// Full-text search weight (default: 0.3)
    #[serde(default = "default_weight_fts")]
    fts: f64,
    /// Symbolic weight (default: 0.2)
    #[serde(default = "default_weight_symbolic")]
    symbolic: f64,
    /// Decay weight (default: 0.1)
    #[serde(default = "default_weight_decay")]
    decay: f64,
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
struct SearchMemoryParams {
    /// Search query text
    query: String,

    /// Optional query embedding vector; when provided, enables vector channel
    #[serde(default)]
    query_vec: Option<Vec<f32>>,

    /// Number of results to return (default: 6)
    #[serde(default = "default_top_k")]
    top_k: usize,

    /// Optional path prefix filter
    #[serde(default)]
    path_prefix: Option<String>,

    /// Whether to include archived entries
    #[serde(default)]
    include_archived: bool,

    /// Number of candidates per channel
    #[serde(default = "default_candidates")]
    candidates_per_channel: usize,

    /// MMR diversity threshold (0.0-1.0), set to null to disable
    #[serde(default = "default_mmr_threshold")]
    mmr_threshold: Option<f64>,

    /// Graph expand hops (0 = disabled, default = 1)
    #[serde(default = "default_graph_hops")]
    graph_expand_hops: u32,

    /// Optional relation filter for graph expansion
    #[serde(default)]
    graph_relation_filter: Option<String>,

    /// Optional scoring weights override {semantic, fts, symbolic, decay}
    #[serde(default)]
    weights: Option<HybridWeightsParam>,
}

impl SearchMemoryParams {
    /// Build SearchOptions from params, only differing by vec_available per DB.
    fn to_search_options(&self, vec_available: bool) -> SearchOptions {
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
            ..Default::default()
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct FindSimilarMemoryParams {
    /// Query embedding vector (same dimension as stored embeddings)
    query_vec: Vec<f32>,

    /// Number of results to return (default: 5)
    #[serde(default = "default_find_similar_top_k")]
    top_k: usize,

    /// Optional path prefix filter
    #[serde(default)]
    path_prefix: Option<String>,

    /// Whether to include archived entries
    #[serde(default)]
    include_archived: bool,

    /// Number of candidates pulled from vector channel (default: 20)
    #[serde(default = "default_candidates")]
    candidates_per_channel: usize,
}

fn default_find_similar_top_k() -> usize {
    5
}

/// Build a MemoryEntry from a JSON fact value (shared by extract_facts and ingest_event).
fn fact_to_entry(
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
struct GetMemoryParams {
    /// Memory entry ID
    id: String,

    /// Whether to include archived entries
    #[serde(default)]
    include_archived: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ListMemoriesParams {
    /// Path prefix to filter
    #[serde(default = "default_path")]
    path_prefix: String,

    /// Maximum number of entries to return
    #[serde(default = "default_limit")]
    limit: usize,

    /// Whether to include archived entries
    #[serde(default)]
    include_archived: bool,
}

#[allow(dead_code)]
fn default_limit() -> usize {
    100
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SetStateParams {
    /// State key
    key: String,

    /// State value (JSON value)
    value: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct GetStateParams {
    /// State key
    key: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ExtractFactsParams {
    /// Text to extract facts from
    text: String,

    /// Source identifier for the extraction
    #[serde(default = "default_extraction_source")]
    source: String,
}

#[allow(dead_code)]
fn default_extraction_source() -> String {
    "extraction".to_string()
}

/// A single message in a conversation turn
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct Message {
    /// Role of the message sender (e.g., "user", "assistant", "system")
    #[allow(dead_code)]
    role: String,
    /// Content of the message
    content: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct IngestEventParams {
    /// Conversation identifier
    conversation_id: String,

    /// Turn identifier
    turn_id: String,

    /// Messages in the conversation turn
    messages: Vec<Message>,
}

#[allow(dead_code)]
fn default_hub_version() -> u32 {
    1
}

#[allow(dead_code)]
fn default_true() -> bool {
    true
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubRegisterParams {
    /// Unique capability ID, e.g. "skill:code-review", "mcp:github"
    id: String,
    /// Type: "skill" | "plugin" | "mcp"
    cap_type: String,
    /// Human-readable name
    name: String,
    /// Short description
    #[serde(default)]
    description: String,
    /// JSON string of capability definition (prompt template, manifest, config)
    #[serde(default)]
    definition: String,
    /// Version number
    #[serde(default = "default_hub_version")]
    version: u32,
    /// Target database scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    scope: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubDiscoverParams {
    /// Optional search query (searches name + description)
    #[serde(default)]
    query: Option<String>,
    /// Optional type filter: "skill" | "plugin" | "mcp"
    #[serde(default)]
    cap_type: Option<String>,
    /// Only return enabled capabilities (default: true)
    #[serde(default = "default_true")]
    enabled_only: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubGetParams {
    /// Capability ID to retrieve
    id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubFeedbackParams {
    /// Capability ID
    id: String,
    /// Whether the invocation was successful
    success: bool,
    /// Optional user rating (0.0 - 5.0)
    #[serde(default)]
    rating: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubCallParams {
    /// MCP server capability ID (e.g. "mcp:github")
    server_id: String,
    /// Tool name to call on the child MCP server
    tool_name: String,
    /// JSON arguments to pass to the tool
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HubDisconnectParams {
    /// MCP server capability ID (e.g. "mcp:longbridge") or server name
    server_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DeleteMemoryParams {
    /// Memory entry ID to delete
    id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ArchiveMemoryParams {
    /// Memory entry ID to archive
    id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HubSetEnabledParams {
    /// Capability ID
    id: String,
    /// Whether to enable (true) or disable (false)
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct AddEdgeParams {
    /// Source memory ID
    source_id: String,
    /// Target memory ID
    target_id: String,
    /// Relation type (e.g. "causes", "follows", "related_to")
    relation: String,
    /// Edge weight (default: 1.0)
    #[serde(default = "default_edge_weight")]
    weight: f64,
    /// Optional JSON metadata for the edge
    #[serde(default)]
    metadata: Option<serde_json::Value>,

    /// Scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    scope: String,
}

fn default_edge_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct GetEdgesParams {
    /// Memory entry ID
    memory_id: String,
    /// Direction: "outgoing", "incoming", or "both" (default: "both")
    #[serde(default = "default_edge_direction")]
    direction: String,
    /// Optional relation type filter
    #[serde(default)]
    relation_filter: Option<String>,

    /// Scope: "global" or "project" (default)
    #[serde(default = "default_scope")]
    scope: String,
}

fn default_edge_direction() -> String {
    "both".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct AuditLogParams {
    /// Maximum entries to return (default: 50)
    #[serde(default = "default_audit_limit")]
    limit: usize,
    /// Optional server_id filter
    #[serde(default)]
    server_filter: Option<String>,
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
struct SyncMemoriesParams {
    /// Unique agent identifier for tracking known state
    agent_id: String,
    /// Optional path prefix to scope the sync (e.g. "/project")
    #[serde(default)]
    path_prefix: Option<String>,
    /// Maximum entries to return (default: 100)
    #[serde(default = "default_sync_limit")]
    limit: usize,
}

// ─── Ghost Whispers (Inter-Agent Pub/Sub) ─────────────────────────────────────

const PUBSUB_RING_MAX: usize = 100;
const PUBSUB_MAX_CURSORS: usize = 1000;

#[derive(Clone)]
struct PubSubMessage {
    topic: String,
    payload: serde_json::Value,
    publisher: String,
    timestamp: String,
    id: String,
}

struct PubSubState {
    /// topic → ring buffer of messages (max PUBSUB_RING_MAX per topic)
    messages: HashMap<String, VecDeque<PubSubMessage>>,
    /// Global monotonic index per topic (total messages ever published)
    next_index: HashMap<String, usize>,
    /// agent_id → (topic → last_seen_global_index)
    cursors: HashMap<String, HashMap<String, usize>>,
    /// agent_id → monotonic recency sequence (used for LRU cursor eviction)
    cursor_recency: HashMap<String, u64>,
    /// Monotonic sequence counter for cursor recency tracking
    cursor_seq: u64,
}

impl PubSubState {
    fn new() -> Self {
        Self {
            messages: HashMap::new(),
            next_index: HashMap::new(),
            cursors: HashMap::new(),
            cursor_recency: HashMap::new(),
            cursor_seq: 0,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct GhostPublishParams {
    /// Topic to publish to (e.g. "build-status", "code-review")
    topic: String,
    /// Message payload (any JSON value)
    payload: serde_json::Value,
    /// Publisher agent identifier
    publisher: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct GhostSubscribeParams {
    /// Unique agent identifier for cursor tracking
    agent_id: String,
    /// Topics to subscribe to and poll for new messages
    topics: Vec<String>,
}

// ─── Dead Letter Queue (Failed Tool Call Auto-Retry) ─────────────────────────

const DLQ_MAX_ENTRIES: usize = 200;
const DLQ_TTL_SECS: u64 = 3600; // 1 hour

#[derive(Clone)]
struct DeadLetter {
    id: String,
    tool_name: String,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    error: String,
    error_category: String, // "invalid_params", "timeout", "internal", "not_found"
    timestamp: String,
    retry_count: u32,
    max_retries: u32,
    status: String, // "pending", "retrying", "resolved", "abandoned"
}

fn categorize_error(error: &str) -> String {
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

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DlqListParams {
    /// Filter by status: "pending", "retrying", "resolved", "abandoned"
    #[serde(default)]
    status_filter: Option<String>,
    /// Max entries to return (default: 50)
    #[serde(default)]
    limit: Option<usize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct DlqRetryParams {
    /// ID of the dead letter entry to retry
    dead_letter_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SandboxSetRuleParams {
    /// Agent role (e.g. "code-review", "finance", "admin")
    agent_role: String,
    /// Path pattern to match (e.g. "/finance/*", "/project/secrets")
    path_pattern: String,
    /// Access level: "read", "write", or "deny"
    #[serde(default = "default_access_level")]
    access_level: String,
}

fn default_access_level() -> String {
    "read".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SandboxCheckParams {
    /// Agent role to check access for
    agent_role: String,
    /// Memory path to check
    path: String,
    /// Operation type: "read" or "write"
    #[serde(default = "default_sandbox_operation")]
    operation: String,
}

fn default_sandbox_operation() -> String {
    "read".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ChainStep {
    /// Skill capability ID (e.g. "skill:summarize")
    skill_id: String,
    /// Extra arguments to merge with piped input
    #[serde(default)]
    extra_args: Option<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ChainSkillsParams {
    /// Ordered list of skill steps to execute
    steps: Vec<ChainStep>,
    /// Input for the first step
    initial_input: String,
}

// ─── Tool Implementations ────────────────────────────────────────────────────────

#[tool_router]
impl MemoryServer {
    #[tool(
        description = "Save a memory entry to the store. Creates a new entry or updates an existing one if id is provided."
    )]
    async fn save_memory(
        &self,
        Parameters(params): Parameters<SaveMemoryParams>,
    ) -> Result<String, String> {
        // Noise filter: reject junk text before persisting unless force-save is requested.
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
        let (target_db, warning) = self.resolve_write_scope(&requested_scope);

        // Use caller-provided summary or leave empty for background enrichment
        let summary = params.summary;
        let needs_summary = summary.is_empty();
        let needs_embedding = params.vector.is_none();

        let entry = MemoryEntry {
            id: id.clone(),
            path: params.path,
            summary,
            text: params.text,
            importance: params.importance,
            timestamp: timestamp.clone(),
            category: params.category,
            topic: params.topic,
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
            metadata: serde_json::json!({}),
            vector: params.vector,
        };

        // Write immediately with empty summary/vector
        self.with_store_for_scope(target_db, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory: {}", e))
        })?;

        // Spawn background enrichment (embedding + summary)
        // Uses revision-aware update to avoid overwriting concurrent changes
        let server = self.clone();
        let enrich_id = id.clone();
        let enrich_text = entry.text.clone();
        let enrich_revision = 1_i64; // initial revision at time of save
        tokio::spawn(async move {
            let mut new_vec: Option<Vec<f32>> = None;
            let mut new_summary: Option<String> = None;

            // Generate embedding
            if needs_embedding {
                match server.llm.embed_voyage(&enrich_text, "document").await {
                    Ok(vec) => new_vec = Some(vec),
                    Err(e) => eprintln!("[enrichment] embedding failed for {enrich_id}: {e}"),
                }
            }

            // Generate summary if not provided
            if needs_summary {
                match server.llm.generate_summary(&enrich_text).await {
                    Ok(s) => new_summary = Some(s),
                    Err(e) => eprintln!("[enrichment] summary failed for {enrich_id}: {e}"),
                }
            }

            // Revision-aware targeted update — discards enrichment if entry was modified
            if new_vec.is_some() || new_summary.is_some() {
                match server.with_store_for_scope(target_db, |store| {
                    store
                        .update_enrichment_fields(
                            &enrich_id,
                            new_summary.as_deref(),
                            new_vec.as_deref(),
                            enrich_revision,
                        )
                        .map_err(|e| format!("Failed to update enriched entry: {e}"))
                }) {
                    Ok(true) => eprintln!("[enrichment] completed for {enrich_id}"),
                    Ok(false) => {
                        eprintln!("[enrichment] discarded for {enrich_id} (revision changed)")
                    }
                    Err(e) => eprintln!("[enrichment] DB update failed for {enrich_id}: {e}"),
                }
            }
        });

        let mut response = serde_json::Map::new();
        response.insert("id".into(), json!(id));
        response.insert("timestamp".into(), json!(timestamp));
        response.insert("db".into(), json!(target_db.as_str()));
        response.insert("status".into(), json!("saved (enrichment pending)"));
        if let Some(warning) = warning {
            response.insert("warning".into(), json!(warning));
        }

        // 6d: Auto-link — create edges to memories sharing the same entities
        if params.auto_link && !entry.entities.is_empty() {
            let auto_link_server = self.clone();
            let auto_link_id = id.clone();
            let auto_link_entities = entry.entities.clone();
            tokio::spawn(async move {
                for entity in &auto_link_entities {
                    // Search for existing memories mentioning this entity
                    let query = entity.clone();
                    if let Ok(results) = auto_link_server.with_global_store(|store| {
                        store
                            .search(
                                &query,
                                Some(memory_core::SearchOptions {
                                    top_k: 5,
                                    ..Default::default()
                                }),
                            )
                            .map_err(|e| format!("{}", e))
                    }) {
                        for result in results {
                            if result.entry.id == auto_link_id {
                                continue;
                            }
                            // Only link if at least one entity overlaps
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
                                let _ = auto_link_server.with_global_store(|store| {
                                    store.add_edge(&edge).map_err(|e| format!("{}", e))
                                });
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

    #[tool(
        description = "Search memory entries using hybrid search (vector + FTS + symbolic). Returns ranked results with scores."
    )]
    async fn search_memory(
        &self,
        Parameters(params): Parameters<SearchMemoryParams>,
    ) -> Result<String, String> {
        // Adaptive retrieval: skip noise queries
        if memory_core::should_skip_query(&params.query) {
            return serde_json::to_string(&json!([]))
                .map_err(|e| format!("Failed to serialize: {}", e));
        }

        let pipeline_enabled = self.pipeline_enabled;

        let mut combined_results: Vec<(memory_core::SearchResult, DbScope)> = Vec::new();

        let global_opts = params.to_search_options(self.global_vec_available);

        let global_results = self.with_global_store(|store| {
            store
                .search(&params.query, Some(global_opts))
                .map_err(|e| format!("Search failed in global DB: {}", e))
        })?;
        combined_results.extend(global_results.into_iter().map(|r| (r, DbScope::Global)));

        if self.project_db_path.is_some() {
            let project_opts = params.to_search_options(self.project_vec_available);

            let project_results = self.with_project_store(|store| {
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

        let mut output: Vec<serde_json::Value> = deduped_results
            .iter()
            .map(|(r, db_scope)| slim_search_result(r, *db_scope))
            .collect();

        if pipeline_enabled {
            let mut existing_ids: HashSet<String> = deduped_results
                .iter()
                .map(|(r, _)| r.entry.id.clone())
                .collect();

            if self.project_db_path.is_some() {
                let project_rules = self.with_project_store(|store| {
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

            let global_rules = self.with_global_store(|store| {
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

        serde_json::to_string(&output).map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(
        description = "Find memory entries similar to a provided vector. Uses vector similarity only (no FTS/symbolic/decay weighting)."
    )]
    async fn find_similar_memory(
        &self,
        Parameters(params): Parameters<FindSimilarMemoryParams>,
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
            vec_available: self.global_vec_available,
            record_access: false,
            include_archived: params.include_archived,
            mmr_threshold: None,
            graph_expand_hops: 0,
            graph_relation_filter: None,
        };

        let global_results = self.with_global_store(|store| {
            store
                .search("", Some(global_opts))
                .map_err(|e| format!("Vector search failed in global DB: {}", e))
        })?;
        combined_results.extend(global_results.into_iter().map(|r| (r, DbScope::Global)));

        if self.project_db_path.is_some() {
            let project_opts = SearchOptions {
                candidates_per_channel: params.candidates_per_channel.max(params.top_k),
                top_k: params.top_k,
                weights: common_weights,
                path_prefix: params.path_prefix.clone(),
                query_vec: Some(params.query_vec.clone()),
                vec_available: self.project_vec_available,
                record_access: false,
                include_archived: params.include_archived,
                mmr_threshold: None,
                graph_expand_hops: 0,
                graph_relation_filter: None,
            };

            let project_results = self.with_project_store(|store| {
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

    #[tool(description = "Get a single memory entry by ID.")]
    async fn get_memory(
        &self,
        Parameters(params): Parameters<GetMemoryParams>,
    ) -> Result<String, String> {
        if self.project_db_path.is_some() {
            let project_entry = self.with_project_store(|store| {
                store
                    .get_with_options(&params.id, params.include_archived)
                    .map_err(|e| format!("Failed to get memory from project DB: {}", e))
            })?;

            if let Some(entry) = project_entry {
                return serde_json::to_string(&slim_entry(&entry, DbScope::Project))
                    .map_err(|e| format!("Failed to serialize: {}", e));
            }
        }

        let global_entry = self.with_global_store(|store| {
            store
                .get_with_options(&params.id, params.include_archived)
                .map_err(|e| format!("Failed to get memory from global DB: {}", e))
        })?;

        match global_entry {
            Some(entry) => serde_json::to_string(&slim_entry(&entry, DbScope::Global))
                .map_err(|e| format!("Failed to serialize: {}", e)),
            None => serde_json::to_string(&json!({
                "error": "Memory not found"
            }))
            .map_err(|e| format!("Failed to serialize: {}", e)),
        }
    }

    #[tool(description = "List memory entries under a path prefix.")]
    async fn list_memories(
        &self,
        Parameters(params): Parameters<ListMemoriesParams>,
    ) -> Result<String, String> {
        let mut combined_entries: Vec<(MemoryEntry, DbScope)> = Vec::new();

        let global_entries = self.with_global_store(|store| {
            store
                .list_by_path(&params.path_prefix, params.limit, params.include_archived)
                .map_err(|e| format!("Failed to list memories from global DB: {}", e))
        })?;
        combined_entries.extend(global_entries.into_iter().map(|e| (e, DbScope::Global)));

        if self.project_db_path.is_some() {
            let project_entries = self.with_project_store(|store| {
                store
                    .list_by_path(&params.path_prefix, params.limit, params.include_archived)
                    .map_err(|e| format!("Failed to list memories from project DB: {}", e))
            })?;
            combined_entries.extend(project_entries.into_iter().map(|e| (e, DbScope::Project)));
        }

        combined_entries.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));
        combined_entries.truncate(params.limit);

        let slim: Vec<serde_json::Value> = combined_entries
            .iter()
            .map(|(e, db_scope)| slim_entry(e, *db_scope))
            .collect();
        serde_json::to_string(&slim).map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(description = "Get aggregate statistics about the memory store.")]
    async fn memory_stats(&self) -> Result<String, String> {
        let global_stats = self.with_global_store(|store| {
            store
                .stats(false)
                .map_err(|e| format!("Failed to get global stats: {}", e))
        })?;

        let project_stats = if self.project_db_path.is_some() {
            Some(self.with_project_store(|store| {
                store
                    .stats(false)
                    .map_err(|e| format!("Failed to get project stats: {}", e))
            })?)
        } else {
            None
        };

        let mut total = global_stats.total;
        let mut by_scope: HashMap<String, u64> = global_stats.by_scope.clone();
        let mut by_category: HashMap<String, u64> = global_stats.by_category.clone();
        let mut by_root_path: HashMap<String, u64> = global_stats.by_root_path.clone();

        if let Some(ref project_stats) = project_stats {
            total += project_stats.total;

            for (k, v) in &project_stats.by_scope {
                *by_scope.entry(k.clone()).or_insert(0) += v;
            }
            for (k, v) in &project_stats.by_category {
                *by_category.entry(k.clone()).or_insert(0) += v;
            }
            for (k, v) in &project_stats.by_root_path {
                *by_root_path.entry(k.clone()).or_insert(0) += v;
            }
        }

        let mut databases = serde_json::Map::new();
        databases.insert(
            "global".into(),
            json!({
                "path": self.global_db_path.display().to_string(),
                "vec_available": self.global_vec_available,
                "total": global_stats.total,
                "by_scope": global_stats.by_scope,
                "by_category": global_stats.by_category,
            }),
        );
        if let Some(ref ps) = project_stats {
            databases.insert(
                "project".into(),
                json!({
                    "path": self.project_db_path.as_ref().map(|p| p.display().to_string()),
                    "vec_available": self.project_vec_available,
                    "total": ps.total,
                    "by_scope": ps.by_scope,
                    "by_category": ps.by_category,
                }),
            );
        }

        serde_json::to_string(&json!({
            "total": total,
            "by_scope": by_scope,
            "by_category": by_category,
            "by_root_path": by_root_path,
            "databases": databases,
        }))
        .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(
        description = "Delete a memory entry permanently. Removes from main table, FTS, vectors, graph edges, and access history."
    )]
    async fn delete_memory(
        &self,
        Parameters(params): Parameters<DeleteMemoryParams>,
    ) -> Result<String, String> {
        // Try project DB first, then global
        if self.project_db_path.is_some() {
            let deleted = self.with_project_store(|store| {
                store
                    .delete(&params.id)
                    .map_err(|e| format!("Delete failed: {}", e))
            })?;
            if deleted {
                return serde_json::to_string(
                    &json!({ "deleted": true, "db": "project", "id": params.id }),
                )
                .map_err(|e| format!("Failed to serialize: {}", e));
            }
        }

        let deleted = self.with_global_store(|store| {
            store
                .delete(&params.id)
                .map_err(|e| format!("Delete failed: {}", e))
        })?;

        serde_json::to_string(&json!({
            "deleted": deleted,
            "db": if deleted { "global" } else { "not_found" },
            "id": params.id,
        }))
        .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(
        description = "Archive a memory entry (soft-delete, set archived=1). Entry is hidden from default searches but can be retrieved with include_archived=true."
    )]
    async fn archive_memory(
        &self,
        Parameters(params): Parameters<ArchiveMemoryParams>,
    ) -> Result<String, String> {
        // Try project DB first, then global
        if self.project_db_path.is_some() {
            let archived = self.with_project_store(|store| {
                store
                    .archive_memory(&params.id)
                    .map_err(|e| format!("Archive failed: {}", e))
            })?;
            if archived {
                return serde_json::to_string(
                    &json!({ "archived": true, "db": "project", "id": params.id }),
                )
                .map_err(|e| format!("Failed to serialize: {}", e));
            }
        }

        let archived = self.with_global_store(|store| {
            store
                .archive_memory(&params.id)
                .map_err(|e| format!("Archive failed: {}", e))
        })?;

        serde_json::to_string(&json!({
            "archived": archived,
            "db": if archived { "global" } else { "not_found" },
            "id": params.id,
        }))
        .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(
        description = "Run garbage collection on growing tables. Prunes old access_history (keep latest 256 per memory), processed_events (30d), audit_log (30d + 100k cap), and agent_known_state (90d)."
    )]
    async fn memory_gc(&self) -> Result<String, String> {
        let mut results = serde_json::Map::new();

        let global_gc = self.with_global_store(|store| {
            store
                .gc_tables()
                .map_err(|e| format!("GC failed on global DB: {}", e))
        })?;
        results.insert("global".into(), global_gc);

        if self.project_db_path.is_some() {
            let project_gc = self.with_project_store(|store| {
                store
                    .gc_tables()
                    .map_err(|e| format!("GC failed on project DB: {}", e))
            })?;
            results.insert("project".into(), project_gc);
        }

        serde_json::to_string(&results).map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(description = "Enable or disable a Hub capability by ID.")]
    async fn hub_set_enabled(
        &self,
        Parameters(params): Parameters<HubSetEnabledParams>,
    ) -> Result<String, String> {
        // Try both DBs — project first
        let mut updated = false;
        let mut target_db = "not_found";

        if self.project_db_path.is_some() {
            let result = self.with_project_store(|store| {
                store
                    .hub_set_enabled(&params.id, params.enabled)
                    .map_err(|e| format!("Failed: {}", e))
            })?;
            if result {
                updated = true;
                target_db = "project";
            }
        }

        if !updated {
            let result = self.with_global_store(|store| {
                store
                    .hub_set_enabled(&params.id, params.enabled)
                    .map_err(|e| format!("Failed: {}", e))
            })?;
            if result {
                updated = true;
                target_db = "global";
            }
        }

        if updated {
            if let Some(server_name) = params.id.strip_prefix("mcp:") {
                if params.enabled {
                    if let Ok(cap) = self.get_capability(&params.id) {
                        if let Ok(def) = serde_json::from_str::<serde_json::Value>(&cap.definition)
                        {
                            if let Some(tools_json) = def.get("discovered_tools") {
                                if let Ok(tools) = serde_json::from_value::<Vec<rmcp::model::Tool>>(
                                    tools_json.clone(),
                                ) {
                                    self.cache_proxy_tools(
                                        server_name,
                                        filter_mcp_tools_by_permissions(&def, tools),
                                    );
                                }
                            }
                        }
                    }
                } else {
                    self.clear_proxy_tools(server_name);
                }
            }
        }

        serde_json::to_string(&json!({
            "updated": updated,
            "db": target_db,
            "id": params.id,
            "enabled": params.enabled,
        }))
        .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(
        description = "Add or update an edge in the memory graph. Edges represent causal, temporal, or entity relationships between memories."
    )]
    async fn add_edge(
        &self,
        Parameters(params): Parameters<AddEdgeParams>,
    ) -> Result<String, String> {
        let requested_scope = params.scope.clone();
        let (target_db, warning) = self.resolve_write_scope(&requested_scope);

        let edge = memory_core::MemoryEdge {
            source_id: params.source_id.clone(),
            target_id: params.target_id.clone(),
            relation: params.relation.clone(),
            weight: params.weight,
            metadata: params.metadata.unwrap_or(json!({})),
            created_at: Utc::now().to_rfc3339(),
            valid_from: String::new(),
            valid_to: None,
        };

        self.with_store_for_scope(target_db, |store| {
            store
                .add_edge(&edge)
                .map_err(|e| format!("Failed to add edge: {}", e))
        })?;

        let mut resp = serde_json::Map::new();
        resp.insert("ok".into(), json!(true));
        resp.insert("db".into(), json!(target_db.as_str()));
        resp.insert("source_id".into(), json!(params.source_id));
        resp.insert("target_id".into(), json!(params.target_id));
        resp.insert("relation".into(), json!(params.relation));
        if let Some(w) = warning {
            resp.insert("warning".into(), json!(w));
        }

        serde_json::to_string(&resp).map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(
        description = "Get edges connected to a memory entry. Returns causal, temporal, and entity relationship edges."
    )]
    async fn get_edges(
        &self,
        Parameters(params): Parameters<GetEdgesParams>,
    ) -> Result<String, String> {
        let (target_db, _warning) = self.resolve_write_scope(&params.scope);

        let edges = self.with_store_for_scope(target_db, |store| {
            store
                .get_edges(
                    &params.memory_id,
                    &params.direction,
                    params.relation_filter.as_deref(),
                )
                .map_err(|e| format!("Failed to get edges: {}", e))
        })?;

        let output: Vec<serde_json::Value> = edges
            .iter()
            .map(|e| {
                json!({
                    "db": target_db.as_str(),
                    "source_id": e.source_id,
                    "target_id": e.target_id,
                    "relation": e.relation,
                    "weight": e.weight,
                    "metadata": e.metadata,
                    "created_at": e.created_at,
                })
            })
            .collect();

        serde_json::to_string(&output).map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(description = "Set a key-value pair in server state (stored in hard_state table).")]
    async fn set_state(
        &self,
        Parameters(params): Parameters<SetStateParams>,
    ) -> Result<String, String> {
        let value_json = serde_json::to_string(&params.value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;

        self.with_global_store(|store| {
            let version = store
                .set_state("mcp", &params.key, &value_json)
                .map_err(|e| format!("Failed to set state: {}", e))?;

            serde_json::to_string(&json!({
                "key": params.key,
                "value": params.value,
                "version": version
            }))
            .map_err(|e| format!("Failed to serialize response: {}", e))
        })
    }

    #[tool(description = "Get a value from server state by key.")]
    async fn get_state(
        &self,
        Parameters(params): Parameters<GetStateParams>,
    ) -> Result<String, String> {
        self.with_global_store(|store| match store.get_state_kv("mcp", &params.key) {
            Ok(Some((value, version))) => {
                let parsed_value: serde_json::Value =
                    serde_json::from_str(&value).unwrap_or_else(|_| serde_json::json!(value));

                serde_json::to_string(&json!({
                    "key": params.key,
                    "value": parsed_value,
                    "version": version
                }))
                .map_err(|e| format!("Failed to serialize: {}", e))
            }
            Ok(None) => serde_json::to_string(&json!({
                "key": params.key,
                "error": "not found"
            }))
            .map_err(|e| format!("Failed to serialize: {}", e)),
            Err(e) => Err(format!("Failed to get state: {}", e)),
        })
    }

    #[tool(description = "Extract structured facts from text using LLM and save to memory.")]
    async fn extract_facts(
        &self,
        Parameters(params): Parameters<ExtractFactsParams>,
    ) -> Result<String, String> {
        let (target_db, _warning) = self.resolve_write_scope("project");
        let source = params.source.clone();

        // Synchronous extraction — caller sees actual results or errors
        let facts = self
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
        let saved = self
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

    #[tool(description = "Ingest a conversation event and extract facts from messages.")]
    async fn ingest_event(
        &self,
        Parameters(params): Parameters<IngestEventParams>,
    ) -> Result<String, String> {
        // Stable hash: deterministic key from conversation+turn IDs (no DefaultHasher)
        let event_hash = stable_hash(&format!("{}:{}", params.conversation_id, params.turn_id));
        let (target_db, _warning) = self.resolve_write_scope("project");

        // Concatenate message contents for fact extraction
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

        // Atomic dedup: try to claim the event first via INSERT OR IGNORE.
        // If another concurrent call already claimed it, we skip.
        let claimed = self.with_store_for_scope(target_db, |store| {
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

        // Spawn LLM fact extraction in background
        let server = self.clone();
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
                        Ok(n) => eprintln!("[ingest_event] saved {n}/{count} facts for {conversation_id}:{turn_id}"),
                        Err(e) => {
                            eprintln!("[ingest_event] DB write failed: {e} — releasing claim for retry");
                            let _ = server.with_store_for_scope(target_db, |store| {
                                store.release_event_claim(&eh, "ingest")
                                    .map_err(|e| format!("{e}"))
                            });
                            // Push to DLQ so failures are visible via dlq_list
                            let dl = DeadLetter {
                                id: uuid::Uuid::new_v4().to_string(),
                                tool_name: "ingest_event".to_string(),
                                arguments: Some(serde_json::Map::from_iter([
                                    ("conversation_id".to_string(), serde_json::json!(conversation_id)),
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
                                let mut dlq = server.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
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
                    // Push to DLQ so failures are visible via dlq_list
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

    #[tool(description = "Get pipeline status and statistics.")]
    async fn get_pipeline_status(&self) -> Result<String, String> {
        let global_stats = self.with_global_store(|store| {
            store
                .stats(false)
                .map_err(|e| format!("Failed to get global stats: {}", e))
        })?;

        let project_stats = if self.project_db_path.is_some() {
            Some(self.with_project_store(|store| {
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

        let cache_size = self
            .tool_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        let hits = self.cache_hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self.cache_misses.load(std::sync::atomic::Ordering::Relaxed);

        // DLQ stats
        let (dlq_total, dlq_pending, dlq_resolved, dlq_abandoned) = {
            let dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
            let total = dlq.len();
            let pending = dlq.iter().filter(|d| d.status == "pending").count();
            let resolved = dlq.iter().filter(|d| d.status == "resolved").count();
            let abandoned = dlq.iter().filter(|d| d.status == "abandoned").count();
            (total, pending, resolved, abandoned)
        };

        serde_json::to_string(&json!({
            "status": "running",
            "workers": if self.pipeline_enabled { "rust_async" } else { "disabled" },
            "total_entries": total_entries,
            "by_scope": by_scope,
            "by_category": by_category,
            "vec_available": {
                "global": self.global_vec_available,
                "project": self.project_vec_available,
            },
            "pipeline_enabled": self.pipeline_enabled,
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

    #[tool(
        description = "Get only new or changed memories since last sync for this agent. Returns incremental diff to save tokens. Use agent_id to identify your agent uniquely."
    )]
    async fn sync_memories(
        &self,
        Parameters(params): Parameters<SyncMemoriesParams>,
    ) -> Result<String, String> {
        let path_prefix = params.path_prefix.as_deref().unwrap_or("/");
        let limit = params.limit.min(500);

        // Collect all memories under path_prefix from both DBs
        let mut all_entries: Vec<(MemoryEntry, DbScope)> = Vec::new();

        let global_entries = self.with_global_store(|store| {
            store
                .list_by_path(path_prefix, limit, false)
                .map_err(|e| format!("Failed to list global memories: {}", e))
        })?;
        all_entries.extend(global_entries.into_iter().map(|e| (e, DbScope::Global)));

        if self.project_db_path.is_some() {
            let project_entries = self.with_project_store(|store| {
                store
                    .list_by_path(path_prefix, limit, false)
                    .map_err(|e| format!("Failed to list project memories: {}", e))
            })?;
            all_entries.extend(project_entries.into_iter().map(|e| (e, DbScope::Project)));
        }

        // Sort by timestamp descending and truncate
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

        // Get memory IDs for lookup
        let memory_ids: Vec<String> = all_entries.iter().map(|(e, _)| e.id.clone()).collect();

        // Look up known revisions for this agent (check global DB — that's where sync state lives)
        let known_revisions = self.with_global_store(|store| {
            store
                .get_agent_known_revisions(&params.agent_id, &memory_ids)
                .map_err(|e| format!("Failed to get known revisions: {}", e))
        })?;

        // Compute diff: new or changed entries
        let mut diff_entries: Vec<serde_json::Value> = Vec::new();
        let mut sync_updates: Vec<(String, i64)> = Vec::new();
        let mut new_count = 0u64;
        let mut changed_count = 0u64;

        for (entry, db_scope) in &all_entries {
            let current_rev = entry.revision;
            match known_revisions.get(&entry.id) {
                None => {
                    // New entry — agent hasn't seen it before
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
                    // Changed entry — revision bumped since last sync
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
                    // Unchanged — still update synced_at timestamp
                    sync_updates.push((entry.id.clone(), current_rev));
                }
            }
        }

        // Update agent known state in global DB
        if !sync_updates.is_empty() {
            self.with_global_store(|store| {
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

    #[tool(description = "Post a kanban card from one agent to another.")]
    async fn post_card(
        &self,
        Parameters(params): Parameters<PostCardParams>,
    ) -> Result<String, String> {
        let from_agent = normalize_agent_id(&params.from_agent);
        let to_agent = normalize_agent_id(&params.to_agent);
        if from_agent.is_empty() {
            return Err("from_agent cannot be empty".to_string());
        }
        if to_agent.is_empty() {
            return Err("to_agent cannot be empty".to_string());
        }

        let title = params.title.trim().to_string();
        let body = params.body.trim().to_string();
        if title.is_empty() {
            return Err("title cannot be empty".to_string());
        }
        if body.is_empty() {
            return Err("body cannot be empty".to_string());
        }

        let priority = normalize_card_priority(&params.priority);
        let card_type = normalize_card_type(&params.card_type);
        let status = "open".to_string();
        let now = Utc::now().to_rfc3339();
        let card_id = uuid::Uuid::new_v4().to_string();

        let mut metadata = json!({
            "from_agent": from_agent,
            "to_agent": to_agent,
            "status": status,
            "priority": priority,
            "card_type": card_type,
            "created_at": now,
        });
        if let Some(thread_id) = params
            .thread_id
            .as_ref()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            metadata["thread_id"] = json!(thread_id);
        }

        let entry = MemoryEntry {
            id: card_id.clone(),
            path: format!(
                "/kanban/{}/{}",
                normalize_agent_id(&params.from_agent),
                normalize_agent_id(&params.to_agent)
            ),
            summary: title.clone(),
            text: body.clone(),
            importance: kanban_priority_importance(
                metadata["priority"].as_str().unwrap_or("medium"),
            ),
            timestamp: now,
            category: KANBAN_CATEGORY.to_string(),
            topic: String::new(),
            keywords: vec![],
            persons: vec![],
            entities: vec![
                normalize_agent_id(&params.from_agent),
                normalize_agent_id(&params.to_agent),
            ],
            location: String::new(),
            source: "agent".to_string(),
            scope: "project".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            vector: None,
            metadata: metadata.clone(),
        };

        let (target_db, warning) = self.resolve_write_scope("project");
        self.with_store_for_scope(target_db, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("failed to save kanban card: {e}"))
        })?;

        let classify_enabled = parse_env_bool("KANBAN_CLASSIFY_ENABLED").unwrap_or(false);
        if classify_enabled {
            let db_path = match target_db {
                DbScope::Global => self.global_db_path.clone(),
                DbScope::Project => self
                    .project_db_path
                    .clone()
                    .unwrap_or_else(|| self.global_db_path.clone()),
            };
            let card_id_clone = card_id.clone();
            let body_clone = body.clone();
            let title_clone = title.clone();
            let source_clone = entry.source.clone();
            let metadata_clone = metadata.clone();
            let expected_revision = entry.revision;
            tokio::spawn(async move {
                if let Err(e) = enrich_kanban_card_classification(
                    db_path,
                    card_id_clone,
                    body_clone,
                    title_clone,
                    source_clone,
                    metadata_clone,
                    expected_revision,
                )
                .await
                {
                    eprintln!("[kanban] classification skipped for card: {e}");
                }
            });
        }

        let mut resp = serde_json::Map::new();
        resp.insert("status".into(), json!("posted"));
        resp.insert("card_id".into(), json!(card_id));
        resp.insert("db".into(), json!(target_db.as_str()));
        resp.insert("classification_enqueued".into(), json!(classify_enabled));
        if let Some(w) = warning {
            append_warning(&mut resp, w);
        }
        serde_json::to_string(&serde_json::Value::Object(resp))
            .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Check kanban inbox for a target agent.")]
    async fn check_inbox(
        &self,
        Parameters(params): Parameters<CheckInboxParams>,
    ) -> Result<String, String> {
        let agent_id = normalize_agent_id(&params.agent_id);
        if agent_id.is_empty() {
            return Err("agent_id cannot be empty".to_string());
        }
        let limit = params.limit.clamp(1, 1000);

        let mut cards: Vec<(MemoryEntry, DbScope)> = Vec::new();
        let global_cards = self.with_global_store(|store| {
            store
                .list_by_path(KANBAN_PATH_PREFIX, limit * 4, false)
                .map_err(|e| format!("list global kanban cards failed: {e}"))
        })?;
        cards.extend(
            global_cards
                .into_iter()
                .filter(|entry| card_matches_inbox(entry, &params, &agent_id))
                .map(|entry| (entry, DbScope::Global)),
        );

        if self.project_db_path.is_some() {
            let project_cards = self.with_project_store(|store| {
                store
                    .list_by_path(KANBAN_PATH_PREFIX, limit * 4, false)
                    .map_err(|e| format!("list project kanban cards failed: {e}"))
            })?;
            cards.extend(
                project_cards
                    .into_iter()
                    .filter(|entry| card_matches_inbox(entry, &params, &agent_id))
                    .map(|entry| (entry, DbScope::Project)),
            );
        }

        cards.sort_by(|a, b| {
            let pa = kanban_priority_rank(card_priority(&a.0).as_str());
            let pb = kanban_priority_rank(card_priority(&b.0).as_str());
            pa.cmp(&pb).then_with(|| b.0.timestamp.cmp(&a.0.timestamp))
        });
        cards.truncate(limit);

        let payload: Vec<serde_json::Value> = cards
            .into_iter()
            .map(|(entry, db)| {
                json!({
                    "id": entry.id,
                    "db": db.as_str(),
                    "from_agent": card_from_agent(&entry),
                    "to_agent": card_to_agent(&entry),
                    "status": card_status(&entry).unwrap_or_else(|| "open".to_string()),
                    "priority": card_priority(&entry),
                    "card_type": card_type(&entry),
                    "thread_id": card_metadata_str(&entry, "thread_id"),
                    "title": entry.summary,
                    "body": entry.text,
                    "path": entry.path,
                    "timestamp": entry.timestamp,
                    "topic": entry.topic,
                    "keywords": entry.keywords,
                    "metadata": entry.metadata,
                })
            })
            .collect();

        serde_json::to_string(&json!({
            "agent_id": agent_id,
            "count": payload.len(),
            "cards": payload,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Update status of a kanban card.")]
    async fn update_card(
        &self,
        Parameters(params): Parameters<UpdateCardParams>,
    ) -> Result<String, String> {
        let new_status = normalize_card_status(&params.new_status).ok_or_else(|| {
            "new_status must be one of open|acknowledged|resolved|expired".to_string()
        })?;

        let mut found: Option<(DbScope, MemoryEntry)> = None;
        if self.project_db_path.is_some() {
            if let Ok(Some(entry)) = self.with_project_store(|store| {
                store
                    .get(&params.card_id)
                    .map_err(|e| format!("get project card failed: {e}"))
            }) {
                found = Some((DbScope::Project, entry));
            }
        }
        if found.is_none() {
            if let Some(entry) = self.with_global_store(|store| {
                store
                    .get(&params.card_id)
                    .map_err(|e| format!("get global card failed: {e}"))
            })? {
                found = Some((DbScope::Global, entry));
            }
        }

        let (target_db, mut entry) = found.ok_or_else(|| {
            format!(
                "kanban card '{}' not found in project/global db",
                params.card_id
            )
        })?;
        if entry.category != KANBAN_CATEGORY {
            return Err(format!(
                "memory '{}' is not a kanban card (category={})",
                entry.id, entry.category
            ));
        }

        let mut metadata = entry.metadata.clone();
        if !metadata.is_object() {
            metadata = json!({});
        }
        metadata["status"] = json!(new_status);
        metadata["updated_at"] = json!(Utc::now().to_rfc3339());

        if let Some(response_text) = params
            .response_text
            .as_ref()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            let mut replies = metadata
                .get("replies")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            replies.push(json!({
                "timestamp": Utc::now().to_rfc3339(),
                "text": response_text,
            }));
            metadata["replies"] = json!(replies);
            entry.text = format!(
                "{}\n\n[{}] {}",
                entry.text,
                Utc::now().to_rfc3339(),
                response_text
            );
        }

        let updated = self.with_store_for_scope(target_db, |store| {
            store
                .update_with_revision(
                    &entry.id,
                    &entry.text,
                    &entry.summary,
                    &entry.source,
                    &metadata,
                    None,
                    entry.revision,
                )
                .map_err(|e| format!("update card failed: {e}"))
        })?;

        if !updated {
            return Err(format!(
                "kanban card '{}' update rejected due to revision mismatch",
                entry.id
            ));
        }

        serde_json::to_string(&json!({
            "updated": true,
            "db": target_db.as_str(),
            "card_id": entry.id,
            "status": metadata["status"],
            "revision": entry.revision + 1,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Register a capability (skill, plugin, or MCP server) in the Hub.")]
    async fn hub_register(
        &self,
        Parameters(params): Parameters<HubRegisterParams>,
    ) -> Result<String, String> {
        let (target_db, warning) = self.resolve_write_scope(&params.scope);

        let mut resp = serde_json::Map::new();
        resp.insert("id".into(), json!(params.id));
        resp.insert("db".into(), json!(target_db.as_str()));
        resp.insert("version".into(), json!(params.version));
        if let Some(w) = warning {
            append_warning(&mut resp, w);
        }

        let mut cap_definition = params.definition.clone();
        let mut enabled = true;

        if params.cap_type == "mcp" {
            let mut def: serde_json::Value = serde_json::from_str(&params.definition)
                .map_err(|e| format!("invalid mcp definition JSON: {e}"))?;
            let transport_type = def["transport"].as_str().unwrap_or("stdio").to_string();
            let tool_exposure_mode = resolve_mcp_tool_exposure(&def, self.mcp_tool_exposure_mode);
            def["tool_exposure"] = json!(tool_exposure_mode.as_str());
            resp.insert("tool_exposure".into(), json!(tool_exposure_mode.as_str()));

            // Security: validate MCP server commands against allowlist
            let auto_enabled = if transport_type == "stdio" {
                if let Some(cmd) = def["command"].as_str() {
                    is_trusted_command(cmd)
                } else {
                    false
                }
            } else {
                true // SSE/HTTP are URLs, no local exec risk
            };

            let server_name = params
                .id
                .strip_prefix("mcp:")
                .unwrap_or(&params.id)
                .to_string();
            self.clear_proxy_tools(&server_name);

            if !auto_enabled {
                enabled = false;
                clear_mcp_discovery_metadata(&mut def);
                let cmd = def["command"].as_str().unwrap_or("unknown");
                append_warning(
                    &mut resp,
                    format!(
                        "Command '{}' is not in the trusted allowlist. Capability registered but disabled. Use hub_set_enabled to activate after review.",
                        cmd
                    ),
                );
                resp.insert("enabled".into(), json!(false));
                resp.insert("discovery".into(), json!("skipped (capability disabled)"));
            } else {
                match self.discover_mcp_tools(&def).await {
                    Ok(tools) => {
                        let total_tools = tools.len();
                        let filtered_tools = filter_mcp_tools_by_permissions(&def, tools);
                        let tools_discovered = filtered_tools.len();
                        let tools_filtered_out = total_tools.saturating_sub(tools_discovered);

                        set_mcp_discovery_success(&mut def, &filtered_tools);
                        self.cache_proxy_tools(&server_name, filtered_tools);

                        resp.insert("enabled".into(), json!(true));
                        resp.insert("tools_discovered".into(), json!(tools_discovered));
                        resp.insert("tools_total".into(), json!(total_tools));
                        if tools_filtered_out > 0 {
                            resp.insert("tools_filtered_out".into(), json!(tools_filtered_out));
                        }
                        if total_tools > 0 && tools_discovered == 0 {
                            append_warning(
                                &mut resp,
                                "MCP discovery succeeded, but all tools were filtered by permissions",
                            );
                        }
                        if tool_exposure_mode == McpToolExposureMode::Gateway {
                            append_warning(
                                &mut resp,
                                "Direct server__tool exposure is disabled (tool_exposure=gateway); call child tools via hub_call",
                            );
                        }
                    }
                    Err(discovery_error) => {
                        enabled = false;
                        set_mcp_discovery_failure(&mut def, &discovery_error);
                        self.clear_proxy_tools(&server_name);

                        resp.insert("enabled".into(), json!(false));
                        resp.insert("discovery_error".into(), json!(discovery_error.clone()));
                        append_warning(
                            &mut resp,
                            "MCP discovery failed; capability registered as disabled. Fix config and re-register to recover.",
                        );
                    }
                }
            }

            cap_definition = serde_json::to_string(&def)
                .map_err(|e| format!("Failed to serialize MCP definition: {e}"))?;
        }

        let cap = HubCapability {
            id: params.id.clone(),
            cap_type: params.cap_type.clone(),
            name: params.name.clone(),
            version: params.version,
            description: params.description.clone(),
            definition: cap_definition,
            enabled,
            uses: 0,
            successes: 0,
            failures: 0,
            avg_rating: 0.0,
            last_used: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        self.with_store_for_scope(target_db, |store| {
            store
                .hub_register(&cap)
                .map_err(|e| format!("Failed to register: {e}"))
        })?;

        if params.cap_type == "skill" {
            match self.register_skill_tool(&cap) {
                Ok(tool_name) => {
                    resp.insert("tool_name".into(), json!(tool_name));
                }
                Err(e) => {
                    resp.insert("skill_error".into(), json!(e));
                }
            }

            // L0 analysis: async background scan of the prompt template
            let def: serde_json::Value =
                serde_json::from_str(&params.definition).unwrap_or_default();
            if def["prompt"].as_str().is_some() {
                let llm = self.llm.clone();
                let cap_clone = cap.clone();
                let desc_empty = params.description.is_empty();
                let db_path = match target_db {
                    DbScope::Global => self.global_db_path.clone(),
                    DbScope::Project => self
                        .project_db_path
                        .clone()
                        .unwrap_or_else(|| self.global_db_path.clone()),
                };
                let prompt_text = def["prompt"].as_str().unwrap().to_string();

                let cap_id = cap_clone.id.clone();

                tokio::spawn(async move {
                    match llm
                        .call_llm(
                            crate::prompts::SKILL_ANALYSIS_PROMPT,
                            &prompt_text,
                            None,
                            0.3,
                            500,
                        )
                        .await
                    {
                        Ok(analysis_raw) => {
                            let analysis_json: serde_json::Value = serde_json::from_str(
                                llm::LlmClient::strip_code_fence(&analysis_raw),
                            )
                            .unwrap_or(serde_json::json!({"summary": analysis_raw}));

                            // Auto-fill description if it was empty
                            if desc_empty {
                                if let Some(summary) = analysis_json["summary"].as_str() {
                                    let mut updated_cap = cap_clone;
                                    updated_cap.description = summary.to_string();
                                    if let Ok(store) =
                                        MemoryStore::open(db_path.to_str().unwrap_or(""))
                                    {
                                        let _ = store.hub_register(&updated_cap);
                                    }
                                }
                            }
                            eprintln!("[skill-analysis] {}: {:?}", cap_id, analysis_json);
                        }
                        Err(e) => {
                            eprintln!("[skill-analysis] failed for {}: {}", cap_id, e);
                        }
                    }
                });
                resp.insert("analysis".into(), json!("pending (async)"));
            }
        }

        serde_json::to_string(&serde_json::Value::Object(resp))
            .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Discover available capabilities (skills, plugins, MCP servers) in the Hub."
    )]
    async fn hub_discover(
        &self,
        Parameters(params): Parameters<HubDiscoverParams>,
    ) -> Result<String, String> {
        let cap_type = params.cap_type.as_deref();

        let global_caps = self.with_global_store(|store| {
            if let Some(ref q) = params.query {
                store
                    .hub_search(q, cap_type)
                    .map_err(|e| format!("hub search global: {e}"))
            } else {
                store
                    .hub_list(cap_type, params.enabled_only)
                    .map_err(|e| format!("hub list global: {e}"))
            }
        })?;

        let project_caps = if self.project_db_path.is_some() {
            self.with_project_store(|store| {
                if let Some(ref q) = params.query {
                    store
                        .hub_search(q, cap_type)
                        .map_err(|e| format!("hub search project: {e}"))
                } else {
                    store
                        .hub_list(cap_type, params.enabled_only)
                        .map_err(|e| format!("hub list project: {e}"))
                }
            })?
        } else {
            vec![]
        };

        let mut seen = HashSet::new();
        let mut output: Vec<serde_json::Value> = Vec::new();

        for cap in &project_caps {
            seen.insert(cap.id.clone());
            let mut obj = serde_json::to_value(cap).unwrap_or(json!(null));
            if let Some(o) = obj.as_object_mut() {
                o.insert("db".into(), json!("project"));
            }
            output.push(obj);
        }
        for cap in &global_caps {
            if !seen.insert(cap.id.clone()) {
                continue;
            }
            let mut obj = serde_json::to_value(cap).unwrap_or(json!(null));
            if let Some(o) = obj.as_object_mut() {
                o.insert("db".into(), json!("global"));
            }
            output.push(obj);
        }

        serde_json::to_string(&output).map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Get a specific capability from the Hub by ID.")]
    async fn hub_get(
        &self,
        Parameters(params): Parameters<HubGetParams>,
    ) -> Result<String, String> {
        if self.project_db_path.is_some() {
            if let Some(cap) = self.with_project_store(|store| {
                store
                    .hub_get(&params.id)
                    .map_err(|e| format!("hub get project: {e}"))
            })? {
                let mut obj = serde_json::to_value(&cap).unwrap_or(json!(null));
                if let Some(o) = obj.as_object_mut() {
                    o.insert("db".into(), json!("project"));
                }
                return serde_json::to_string(&obj).map_err(|e| format!("serialize: {e}"));
            }
        }
        match self.with_global_store(|store| {
            store
                .hub_get(&params.id)
                .map_err(|e| format!("hub get global: {e}"))
        })? {
            Some(cap) => {
                let mut obj = serde_json::to_value(&cap).unwrap_or(json!(null));
                if let Some(o) = obj.as_object_mut() {
                    o.insert("db".into(), json!("global"));
                }
                serde_json::to_string(&obj).map_err(|e| format!("serialize: {e}"))
            }
            None => serde_json::to_string(&json!({"error": "Capability not found"}))
                .map_err(|e| format!("serialize: {e}")),
        }
    }

    #[tool(description = "Record feedback for a Hub capability invocation.")]
    async fn hub_feedback(
        &self,
        Parameters(params): Parameters<HubFeedbackParams>,
    ) -> Result<String, String> {
        if self.project_db_path.is_some() {
            let found = self.with_project_store(|store| {
                store
                    .hub_get(&params.id)
                    .map_err(|e| format!("hub get: {e}"))
            })?;
            if found.is_some() {
                self.with_project_store(|store| {
                    store
                        .hub_record_feedback(&params.id, params.success, params.rating)
                        .map_err(|e| format!("feedback: {e}"))
                })?;
                return serde_json::to_string(
                    &json!({"id": params.id, "recorded": true, "db": "project"}),
                )
                .map_err(|e| format!("serialize: {e}"));
            }
        }
        self.with_global_store(|store| {
            store
                .hub_record_feedback(&params.id, params.success, params.rating)
                .map_err(|e| format!("feedback: {e}"))
        })?;
        serde_json::to_string(&json!({"id": params.id, "recorded": true, "db": "global"}))
            .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Get Hub capability statistics and metrics.")]
    async fn hub_stats(&self) -> Result<String, String> {
        let global_caps = self.with_global_store(|store| {
            store
                .hub_list(None, false)
                .map_err(|e| format!("hub list: {e}"))
        })?;
        let project_caps = if self.project_db_path.is_some() {
            self.with_project_store(|store| {
                store
                    .hub_list(None, false)
                    .map_err(|e| format!("hub list: {e}"))
            })?
        } else {
            vec![]
        };

        let total = global_caps.len() + project_caps.len();
        let mut by_type: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let all_caps: Vec<&HubCapability> = global_caps.iter().chain(project_caps.iter()).collect();
        for cap in &all_caps {
            *by_type.entry(cap.cap_type.clone()).or_insert(0) += 1;
        }
        let total_uses: u64 = all_caps.iter().map(|c| c.uses).sum();
        let total_successes: u64 = all_caps.iter().map(|c| c.successes).sum();

        serde_json::to_string(&json!({
            "total_capabilities": total,
            "by_type": by_type,
            "total_uses": total_uses,
            "total_successes": total_successes,
            "success_rate": if total_uses > 0 { total_successes as f64 / total_uses as f64 } else { 0.0 },
            "global_count": global_caps.len(),
            "project_count": project_caps.len(),
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Execute a registered Skill from the Hub using the internal LLM pipeline."
    )]
    async fn run_skill(
        &self,
        Parameters(params): Parameters<RunSkillParams>,
    ) -> Result<String, String> {
        let cap = {
            let mut found = None;
            if self.project_db_path.is_some() {
                found = self.with_project_store(|store| {
                    store
                        .hub_get(&params.skill_id)
                        .map_err(|e| format!("hub get project: {e}"))
                })?;
            }
            if found.is_none() {
                found = self.with_global_store(|store| {
                    store
                        .hub_get(&params.skill_id)
                        .map_err(|e| format!("hub get global: {e}"))
                })?;
            }
            found.ok_or_else(|| format!("Skill '{}' not found in Hub", params.skill_id))?
        };

        if cap.cap_type != "skill" {
            return Err(format!(
                "'{}' is type '{}', not 'skill'",
                params.skill_id, cap.cap_type
            ));
        }

        let def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| format!("invalid skill definition JSON: {e}"))?;

        let prompt_template = def["prompt"]
            .as_str()
            .ok_or_else(|| "skill definition missing 'prompt' field".to_string())?;

        let mut resolved_prompt = prompt_template.to_string();
        if let Some(args_obj) = params.args.as_object() {
            for (k, v) in args_obj {
                let placeholder = format!("{{{{{}}}}}", k);
                let val_str = if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                };
                resolved_prompt = resolved_prompt.replace(&placeholder, &val_str);
            }
        }

        self.llm
            .call_llm(
                "You are an AI assistant executing a specialized skill.",
                &resolved_prompt,
                None,
                0.3,
                4000,
            )
            .await
            .map_err(|e| format!("skill execution failed: {}", e))
    }

    #[tool(description = "View audit log of proxy tool calls through the Hub.")]
    async fn tachi_audit_log(
        &self,
        Parameters(params): Parameters<AuditLogParams>,
    ) -> Result<String, String> {
        self.with_global_store(|store| {
            let entries = store
                .audit_log_list(params.limit, params.server_filter.as_deref())
                .map_err(|e| format!("audit log: {e}"))?;
            serde_json::to_string(&entries).map_err(|e| format!("serialize: {e}"))
        })
    }

    #[tool(
        description = "Call a tool on a registered MCP server through the Hub using the shared connection pool."
    )]
    async fn hub_call(
        &self,
        Parameters(params): Parameters<HubCallParams>,
    ) -> Result<String, String> {
        // Check enabled status before calling
        let cap = self
            .get_capability(&params.server_id)
            .map_err(|e| format!("{e}"))?;
        if !cap.enabled {
            return Err(format!(
                "MCP server '{}' is disabled. Use hub_set_enabled to activate after review.",
                params.server_id
            ));
        }

        let server_name = params
            .server_id
            .strip_prefix("mcp:")
            .unwrap_or(&params.server_id);
        let result = self
            .proxy_call_internal(
                server_name,
                &params.tool_name,
                params.arguments.as_object().cloned(),
            )
            .await
            .map_err(|e| format!("{e}"))?;

        let content_texts: Vec<String> = result
            .content
            .iter()
            .filter_map(|c| {
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str().map(String::from)))
            })
            .collect();
        serde_json::to_string(&json!({
            "server": params.server_id,
            "tool": params.tool_name,
            "content": content_texts,
            "is_error": result.is_error.unwrap_or(false),
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Disconnect a cached MCP server connection from the pool. Forces a fresh reconnect (with updated env/config) on next hub_call."
    )]
    async fn hub_disconnect(
        &self,
        Parameters(params): Parameters<HubDisconnectParams>,
    ) -> Result<String, String> {
        let server_name = params
            .server_id
            .strip_prefix("mcp:")
            .unwrap_or(&params.server_id);

        // Remove from connection pool
        let had_connection = {
            let mut conns = self
                .pool
                .connections
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            conns.remove(server_name).is_some()
        };

        // Also clear discovered tools cache
        {
            let mut tools = self.proxy_tools.lock().unwrap_or_else(|e| e.into_inner());
            tools.remove(server_name);
        }

        serde_json::to_string(&json!({
            "server": server_name,
            "disconnected": had_connection,
            "message": if had_connection {
                "Connection dropped. Next hub_call will reconnect with latest config."
            } else {
                "No active connection found (will connect fresh on next hub_call)."
            },
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    // ─── Ghost Whispers (Inter-Agent Pub/Sub) ────────────────────────────────

    #[tool(
        description = "Publish a message to a Ghost Whispers topic. Other agents can poll for new messages via ghost_subscribe."
    )]
    async fn ghost_publish(
        &self,
        Parameters(params): Parameters<GhostPublishParams>,
    ) -> Result<String, String> {
        let msg_id = uuid::Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();

        let msg = PubSubMessage {
            topic: params.topic.clone(),
            payload: params.payload,
            publisher: params.publisher.clone(),
            timestamp: timestamp.clone(),
            id: msg_id.clone(),
        };

        let mut state = self.pubsub.lock().unwrap_or_else(|e| e.into_inner());
        let ring = state
            .messages
            .entry(params.topic.clone())
            .or_insert_with(VecDeque::new);
        ring.push_back(msg);
        if ring.len() > PUBSUB_RING_MAX {
            ring.pop_front();
        }
        let idx = state.next_index.entry(params.topic.clone()).or_insert(0);
        *idx += 1;

        serde_json::to_string(&json!({
            "id": msg_id,
            "topic": params.topic,
            "publisher": params.publisher,
            "timestamp": timestamp,
            "global_index": *idx,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Subscribe to Ghost Whispers topics and get new messages since last poll. Advances the cursor so the same messages are not returned again."
    )]
    async fn ghost_subscribe(
        &self,
        Parameters(params): Parameters<GhostSubscribeParams>,
    ) -> Result<String, String> {
        let mut state = self.pubsub.lock().unwrap_or_else(|e| e.into_inner());

        // Evict least-recently-active cursors if we exceed the limit
        if state.cursors.len() >= PUBSUB_MAX_CURSORS
            && !state.cursors.contains_key(&params.agent_id)
        {
            let evict_agent = state
                .cursor_recency
                .iter()
                .min_by_key(|(_, seq)| *seq)
                .map(|(agent_id, _)| agent_id.clone())
                .or_else(|| state.cursors.keys().next().cloned());

            if let Some(agent_id) = evict_agent {
                state.cursors.remove(&agent_id);
                state.cursor_recency.remove(&agent_id);
            }
        }

        // Read current cursors for this agent (clone to release borrow)
        let prev_cursors: HashMap<String, usize> = state
            .cursors
            .get(&params.agent_id)
            .cloned()
            .unwrap_or_default();

        let mut new_messages: Vec<serde_json::Value> = Vec::new();
        let mut new_cursors: HashMap<String, usize> = prev_cursors.clone();

        for topic in &params.topics {
            let global_idx = state.next_index.get(topic).copied().unwrap_or(0);
            let cursor = prev_cursors.get(topic).copied().unwrap_or(0);

            if cursor >= global_idx {
                new_cursors.insert(topic.clone(), global_idx);
                continue; // no new messages
            }

            if let Some(ring) = state.messages.get(topic) {
                let ring_start_idx = if global_idx >= ring.len() {
                    global_idx - ring.len()
                } else {
                    0
                };
                let skip = if cursor > ring_start_idx {
                    cursor - ring_start_idx
                } else {
                    0
                };

                for msg in ring.iter().skip(skip) {
                    new_messages.push(json!({
                        "id": msg.id,
                        "topic": msg.topic,
                        "payload": msg.payload,
                        "publisher": msg.publisher,
                        "timestamp": msg.timestamp,
                    }));
                }
            }

            // Advance cursor to current head
            new_cursors.insert(topic.clone(), global_idx);
        }

        // Write back updated cursors
        state.cursors.insert(params.agent_id.clone(), new_cursors);
        state.cursor_seq = state.cursor_seq.saturating_add(1);
        let recency_seq = state.cursor_seq;
        state
            .cursor_recency
            .insert(params.agent_id.clone(), recency_seq);

        serde_json::to_string(&json!({
            "agent_id": params.agent_id,
            "new_count": new_messages.len(),
            "messages": new_messages,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "List active Ghost Whispers topics with message counts and last message time."
    )]
    async fn ghost_topics(&self) -> Result<String, String> {
        let state = self.pubsub.lock().unwrap_or_else(|e| e.into_inner());

        let mut topics: Vec<serde_json::Value> = Vec::new();
        for (topic, ring) in &state.messages {
            if ring.is_empty() {
                continue;
            }
            let last_msg = ring.back().unwrap();
            topics.push(json!({
                "topic": topic,
                "count": ring.len(),
                "total_published": state.next_index.get(topic).copied().unwrap_or(0),
                "last_message_time": last_msg.timestamp,
                "last_publisher": last_msg.publisher,
            }));
        }

        serde_json::to_string(&json!({
            "active_topics": topics.len(),
            "topics": topics,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    // ─── Skill Chaining (Unix Pipe-Style Composition) ────────────────────────

    #[tool(
        description = "Execute a chain of skills in sequence (Unix pipe style). Output of each skill feeds as input to the next."
    )]
    async fn chain_skills(
        &self,
        Parameters(params): Parameters<ChainSkillsParams>,
    ) -> Result<String, String> {
        if params.steps.is_empty() {
            return Err("chain_skills requires at least one step".to_string());
        }

        let mut current_input = params.initial_input;
        let mut step_results: Vec<serde_json::Value> = Vec::new();

        for (i, step) in params.steps.iter().enumerate() {
            let start = Instant::now();

            // Build args: merge extra_args with piped input
            let mut args = match &step.extra_args {
                Some(Value::Object(obj)) => Value::Object(obj.clone()),
                _ => json!({}),
            };
            if let Value::Object(ref mut map) = args {
                map.insert("input".into(), json!(current_input));
            }

            let run_params = RunSkillParams {
                skill_id: step.skill_id.clone(),
                args,
            };

            match self.run_skill(Parameters(run_params)).await {
                Ok(output) => {
                    let elapsed_ms = start.elapsed().as_millis();
                    step_results.push(json!({
                        "step": i,
                        "skill_id": step.skill_id,
                        "elapsed_ms": elapsed_ms,
                        "status": "ok",
                    }));
                    current_input = output;
                }
                Err(e) => {
                    step_results.push(json!({
                        "step": i,
                        "skill_id": step.skill_id,
                        "status": "error",
                        "error": e,
                    }));
                    return Err(format!(
                        "chain_skills failed at step {} (skill '{}'): {}",
                        i, step.skill_id, e
                    ));
                }
            }
        }

        serde_json::to_string(&json!({
            "status": "ok",
            "total_steps": params.steps.len(),
            "output": current_input,
            "steps": step_results,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    // ─── Dead Letter Queue Tools ──────────────────────────────────────────────

    #[tool(
        description = "List dead letter queue entries (failed tool calls). Filter by status: pending, retrying, resolved, abandoned."
    )]
    async fn dlq_list(
        &self,
        Parameters(params): Parameters<DlqListParams>,
    ) -> Result<String, String> {
        let limit = params.limit.unwrap_or(50).min(200);
        let now = Utc::now();

        let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());

        // Expire old entries (> 1 hour)
        dlq.retain(|dl| {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&dl.timestamp) {
                (now - ts.with_timezone(&Utc)).num_seconds() < DLQ_TTL_SECS as i64
            } else {
                false
            }
        });

        let entries: Vec<serde_json::Value> = dlq
            .iter()
            .filter(|dl| {
                if let Some(ref filter) = params.status_filter {
                    dl.status == *filter
                } else {
                    true
                }
            })
            .rev() // newest first
            .take(limit)
            .map(|dl| {
                json!({
                    "id": dl.id,
                    "tool_name": dl.tool_name,
                    "error": dl.error,
                    "error_category": dl.error_category,
                    "timestamp": dl.timestamp,
                    "retry_count": dl.retry_count,
                    "max_retries": dl.max_retries,
                    "status": dl.status,
                })
            })
            .collect();

        let total = dlq.len();
        drop(dlq);

        serde_json::to_string(&json!({
            "total": total,
            "returned": entries.len(),
            "entries": entries,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Manually retry a dead letter queue entry by its ID. Re-dispatches the failed tool call."
    )]
    async fn dlq_retry(
        &self,
        Parameters(params): Parameters<DlqRetryParams>,
    ) -> Result<String, String> {
        // Find and extract the dead letter entry
        let dead_letter = {
            let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
            let pos = dlq.iter().position(|dl| dl.id == params.dead_letter_id);
            match pos {
                Some(idx) => {
                    let mut dl = dlq[idx].clone();
                    dl.retry_count += 1;
                    dl.status = "retrying".to_string();
                    dlq[idx] = dl.clone();
                    dl
                }
                None => {
                    return Err(format!(
                        "Dead letter entry '{}' not found",
                        params.dead_letter_id
                    ))
                }
            }
        };

        // Re-dispatch the tool call via shared retry dispatch
        let retry_result = self
            .retry_dispatch(&dead_letter.tool_name, dead_letter.arguments.clone())
            .await;

        match retry_result {
            Ok(res) => {
                // Mark as resolved
                let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(dl) = dlq.iter_mut().find(|dl| dl.id == params.dead_letter_id) {
                    dl.status = "resolved".to_string();
                }
                let text = res
                    .content
                    .first()
                    .and_then(|c| {
                        if let rmcp::model::RawContent::Text(t) = &c.raw {
                            Some(t.text.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "ok".to_string());
                serde_json::to_string(&json!({
                    "status": "resolved",
                    "dead_letter_id": params.dead_letter_id,
                    "result": text,
                }))
                .map_err(|e| format!("serialize: {e}"))
            }
            Err(e) => {
                // Update retry count, abandon if exhausted
                let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(dl) = dlq.iter_mut().find(|dl| dl.id == params.dead_letter_id) {
                    if dl.retry_count >= dl.max_retries {
                        dl.status = "abandoned".to_string();
                    } else {
                        dl.status = "pending".to_string();
                    }
                    dl.error = format!("{e}");
                }
                Err(format!("Retry failed for '{}': {e}", params.dead_letter_id))
            }
        }
    }

    // ─── Semantic Sandboxing Tools ───────────────────────────────────────────

    #[tool(
        description = "Set a sandbox access rule for an agent role + path pattern. Controls which memories a role can access. Access levels: read, write, deny."
    )]
    async fn sandbox_set_rule(
        &self,
        Parameters(params): Parameters<SandboxSetRuleParams>,
    ) -> Result<String, String> {
        // Validate access_level
        if !["read", "write", "deny"].contains(&params.access_level.as_str()) {
            return Err(format!(
                "Invalid access_level '{}'. Must be: read, write, deny",
                params.access_level
            ));
        }

        // Write to global DB (sandbox rules are global)
        self.with_global_store(|store| {
            memory_core::db::set_sandbox_rule(
                store.connection(),
                &params.agent_role,
                &params.path_pattern,
                &params.access_level,
            )
            .map_err(|e| format!("Failed to set sandbox rule: {e}"))
        })?;

        serde_json::to_string(&json!({
            "status": "ok",
            "agent_role": params.agent_role,
            "path_pattern": params.path_pattern,
            "access_level": params.access_level,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Check if an agent role can access a given path for a specific operation. Advisory mode — not enforced in search_memory yet (TODO: future enforcement integration)."
    )]
    async fn sandbox_check(
        &self,
        Parameters(params): Parameters<SandboxCheckParams>,
    ) -> Result<String, String> {
        if !["read", "write"].contains(&params.operation.as_str()) {
            return Err(format!(
                "Invalid operation '{}'. Must be: read, write",
                params.operation
            ));
        }

        let (allowed, matching_rule) = self.with_global_store(|store| {
            memory_core::db::check_sandbox_access(
                store.connection(),
                &params.agent_role,
                &params.path,
                &params.operation,
            )
            .map_err(|e| format!("Failed to check sandbox access: {e}"))
        })?;

        serde_json::to_string(&json!({
            "agent_role": params.agent_role,
            "path": params.path,
            "operation": params.operation,
            "allowed": allowed,
            "matching_rule": matching_rule,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }
}

impl MemoryServer {
    /// Atomically check if connection exists and create if not.
    /// Prevents TOCTOU race where two concurrent calls both spawn a child.
    async fn ensure_child_connected(&self, server_name: &str) -> Result<(), rmcp::ErrorData> {
        // Check under lock
        {
            let conns = self.pool.connections.lock().unwrap();
            if conns.contains_key(server_name) {
                return Ok(());
            }
        }
        // Not connected — acquire connecting lock to serialize connection attempts
        let connecting_lock = {
            let mut locks = self.pool.connecting_locks.lock().unwrap();
            locks
                .entry(server_name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = connecting_lock.lock().await;
        // Double-check after acquiring lock
        {
            let conns = self.pool.connections.lock().unwrap();
            if conns.contains_key(server_name) {
                return Ok(());
            }
        }
        self.connect_child(server_name).await
    }

    async fn connect_child(&self, server_name: &str) -> Result<(), rmcp::ErrorData> {
        let server_id = format!("mcp:{}", server_name);

        let cap = self.get_capability(&server_id)?;
        if !cap.enabled {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "MCP server '{}' is disabled. Use hub_set_enabled to activate after review.",
                    server_id
                ),
                None,
            ));
        }

        let def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("bad definition: {e}"), None))?;
        let startup_timeout_ms = def["startup_timeout_ms"].as_u64().unwrap_or(30_000);
        let startup_timeout = Duration::from_millis(startup_timeout_ms.max(1));
        let client = self
            .connect_mcp_service(&def, startup_timeout)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;

        self.pool.connections.lock().unwrap().insert(
            server_name.to_string(),
            ChildConnection {
                client,
                last_used: Instant::now(),
            },
        );
        Ok(())
    }

    async fn proxy_call_internal(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        // 0. Look up capability for deny-list and timeout config
        let server_id = format!("mcp:{}", server_name);
        let cap = self.get_capability(&server_id)?;
        if !cap.enabled {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "MCP server '{}' is disabled. Use hub_set_enabled to activate after review.",
                    server_id
                ),
                None,
            ));
        }
        let cap_def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("bad definition: {e}"), None))?;

        // 1. Check allow/deny permissions
        if let Some(allow_list) = cap_def["permissions"]["allow"].as_array() {
            let allowed: HashSet<&str> = allow_list.iter().filter_map(|v| v.as_str()).collect();
            if !allowed.is_empty() && !allowed.contains(tool_name) {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Tool '{}' is not in permissions.allow for '{}'",
                        tool_name, server_name
                    ),
                    None,
                ));
            }
        }

        if let Some(deny_list) = cap_def["permissions"]["deny"].as_array() {
            let denied: Vec<&str> = deny_list.iter().filter_map(|v| v.as_str()).collect();
            if denied.contains(&tool_name) {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Tool '{}' is denied by permissions policy on '{}'",
                        tool_name, server_name
                    ),
                    None,
                ));
            }
        }

        // 2. Check circuit breaker
        {
            let mut circuits = self.pool.circuits.lock().unwrap();
            if let Some((state, count)) = circuits.get_mut(server_name) {
                match state {
                    CircuitState::Open { until } => {
                        if Instant::now() < *until {
                            return Err(rmcp::ErrorData::internal_error(
                                format!("Circuit open for '{}', retry after cooldown", server_name),
                                None,
                            ));
                        }
                        *state = CircuitState::HalfOpen;
                        *count = 0;
                    }
                    CircuitState::HalfOpen | CircuitState::Closed => {}
                }
            }
        }

        // 3. Acquire per-child concurrency permit (rebuild if max_concurrency changed)
        let semaphore = {
            let mut sems = self.pool.semaphores.lock().unwrap();
            let max_conc = cap_def["max_concurrency"].as_u64().unwrap_or(1) as usize;
            let needs_rebuild = sems
                .get(server_name)
                .map(|(_, cached_max)| *cached_max != max_conc)
                .unwrap_or(true);
            if needs_rebuild {
                sems.insert(
                    server_name.to_string(),
                    (Arc::new(tokio::sync::Semaphore::new(max_conc)), max_conc),
                );
            }
            sems.get(server_name).unwrap().0.clone()
        };
        let _permit = semaphore
            .acquire()
            .await
            .map_err(|_| rmcp::ErrorData::internal_error("semaphore closed", None))?;

        // 4. Ensure connection exists (atomic check-and-connect to avoid TOCTOU race)
        self.ensure_child_connected(server_name).await?;

        // 5. Get peer and call tool with timeout
        let mut call_params = rmcp::model::CallToolRequestParams::new(tool_name.to_string());
        if let Some(ref args) = arguments {
            call_params = call_params.with_arguments(args.clone());
        }

        let peer = {
            let mut conns = self.pool.connections.lock().unwrap();
            if let Some(conn) = conns.get_mut(server_name) {
                conn.last_used = Instant::now();
                conn.client.peer().clone()
            } else {
                return Err(rmcp::ErrorData::internal_error("connection lost", None));
            }
        };

        let timeout_ms = cap_def["tool_timeout_ms"].as_u64().unwrap_or(30000);
        let start = Instant::now();

        let result = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            peer.call_tool(call_params),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        // 6. Process result, update circuit breaker, log audit
        let final_result = match result {
            Ok(Ok(r)) => {
                // Tool returned successfully (even if r.is_error — that's a tool-level error, not transport)
                let mut circuits = self.pool.circuits.lock().unwrap();
                circuits.insert(server_name.to_string(), (CircuitState::Closed, 0));
                Ok(r)
            }
            Ok(Err(e)) => {
                // Transport/protocol error — increment circuit breaker
                self.record_circuit_failure(server_name);
                Err(rmcp::ErrorData::internal_error(
                    format!("proxy call failed: {e}"),
                    None,
                ))
            }
            Err(_timeout) => {
                // Timeout — increment circuit breaker
                self.record_circuit_failure(server_name);
                Err(rmcp::ErrorData::internal_error(
                    format!(
                        "Tool call '{}' on '{}' timed out after {}ms",
                        tool_name, server_name, timeout_ms
                    ),
                    None,
                ))
            }
        };

        // 7. Audit log (fire and forget)
        let success = final_result.is_ok();
        let error_kind = final_result.as_ref().err().map(|e| format!("{e}"));
        let timestamp = Utc::now().to_rfc3339();
        let args_hash = stable_hash(&format!("{:?}", arguments));
        let _ = self.with_global_store(|store| {
            store
                .audit_log_insert(
                    &timestamp,
                    server_name,
                    tool_name,
                    &args_hash,
                    success,
                    duration_ms,
                    error_kind.as_deref(),
                )
                .map_err(|e| format!("{e}"))
        });

        final_result
    }

    fn record_circuit_failure(&self, server_name: &str) {
        let mut should_remove = false;
        {
            let mut circuits = self.pool.circuits.lock().unwrap();
            let entry = circuits
                .entry(server_name.to_string())
                .or_insert((CircuitState::Closed, 0));
            entry.1 += 1;
            if entry.1 >= 3 || matches!(entry.0, CircuitState::HalfOpen) {
                entry.0 = CircuitState::Open {
                    until: Instant::now() + Duration::from_secs(30),
                };
                should_remove = true;
            }
        }
        if should_remove {
            self.pool.connections.lock().unwrap().remove(server_name);
        }
    }
}

// ─── ServerHandler Implementation ────────────────────────────────────────────────

impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Tachi — memory + Hub for AI agents. Provides hybrid search, memory storage, skill registry, and MCP server proxy.")
    }

    fn list_tools(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let mut tools = self.tool_router.list_all();

            // Add proxy tools from registered MCP servers
            let proxy_snapshot = self
                .proxy_tools
                .lock()
                .map(|proxy| proxy.clone())
                .unwrap_or_default();
            for (server_name, server_tools) in proxy_snapshot {
                let cap_id = format!("mcp:{server_name}");
                let cap = match self.get_capability(&cap_id) {
                    Ok(cap) if cap.enabled => cap,
                    _ => continue,
                };

                let cap_def = serde_json::from_str::<serde_json::Value>(&cap.definition)
                    .unwrap_or_else(|_| json!({}));
                let exposure_mode =
                    resolve_mcp_tool_exposure(&cap_def, self.mcp_tool_exposure_mode);
                if exposure_mode == McpToolExposureMode::Gateway {
                    continue;
                }

                let filtered_tools =
                    filter_mcp_tools_by_permissions(&cap_def, server_tools.clone());

                for tool in filtered_tools {
                    let mut proxied = tool.clone();
                    proxied.name =
                        std::borrow::Cow::Owned(format!("{}__{}", server_name, tool.name));
                    tools.push(proxied);
                }
            }
            // Add skill tools
            if let Ok(skill_defs) = self.skill_tool_defs.lock() {
                tools.extend(skill_defs.values().cloned());
            }

            Ok(rmcp::model::ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        params: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let name = params.name.as_ref();

            // ─── Phantom Tools: cache invalidation on write ops ──────────
            if CACHE_INVALIDATING_TOOLS.contains(&name) {
                if let Ok(mut cache) = self.tool_cache.lock() {
                    cache.clear();
                }
            }

            // ─── Phantom Tools: check cache for read-only tools ──────────
            let is_cacheable = CACHEABLE_TOOLS.contains(&name);
            let cache_key = if is_cacheable {
                let args_str = params
                    .arguments
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                let key = stable_hash(&format!("{}{}", name, args_str));

                // Check cache
                if let Ok(cache) = self.tool_cache.lock() {
                    if let Some(cached) = cache.get(&key) {
                        if cached.created_at.elapsed() < TOOL_CACHE_TTL {
                            self.cache_hits
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return Ok(cached.result.clone());
                        }
                    }
                }
                self.cache_misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(key)
            } else {
                None
            };

            // ─── Dispatch to handler ─────────────────────────────────────
            // Save tool name and arguments for DLQ capture on failure
            let tool_name_owned = name.to_string();
            let tool_args_for_dlq = params.arguments.clone();

            let result = {
                // 1. Native tools first (highest priority)
                if self.tool_router.has_route(name) {
                    let context =
                        rmcp::handler::server::tool::ToolCallContext::new(self, params, context);
                    self.tool_router.call(context).await
                }
                // 2. Skill tools (tachi_skill_*)
                else if self
                    .skill_tools
                    .lock()
                    .map(|map| map.contains_key(name))
                    .unwrap_or(false)
                {
                    self.call_skill_tool(name, params.arguments).await
                }
                // 3. Proxy tools (server__tool pattern)
                else if let Some((server_name, tool_name)) = name.split_once("__") {
                    let exposure_mode = self.proxy_tool_exposure_mode_for_server(server_name)?;
                    if exposure_mode == McpToolExposureMode::Gateway {
                        Err(rmcp::ErrorData::invalid_params(
                            format!(
                                "Direct proxy tools are disabled for '{}'; use hub_call(server_id='mcp:{}', tool_name='{}')",
                                server_name, server_name, tool_name
                            ),
                            None,
                        ))
                    } else {
                        self.proxy_call_internal(server_name, tool_name, params.arguments)
                            .await
                    }
                } else {
                    Err(rmcp::ErrorData::invalid_params("tool not found", None))
                }
            };

            // ─── Dead Letter Queue: capture failures ─────────────────────
            // Skip DLQ for DLQ tools themselves and ghost_* tools
            let is_dlq_exempt = tool_name_owned.starts_with("dlq_")
                || tool_name_owned.starts_with("ghost_")
                || tool_name_owned == "get_pipeline_status";

            if let Err(ref err) = result {
                if !is_dlq_exempt {
                    let error_str = format!("{}", err);
                    let category = categorize_error(&error_str);
                    let should_auto_retry = category == "timeout" || category == "internal";

                    let dl = DeadLetter {
                        id: uuid::Uuid::new_v4().to_string(),
                        tool_name: tool_name_owned.clone(),
                        arguments: tool_args_for_dlq.clone(),
                        error: error_str.clone(),
                        error_category: category.clone(),
                        timestamp: Utc::now().to_rfc3339(),
                        retry_count: 0,
                        max_retries: if should_auto_retry { 1 } else { 3 },
                        status: "pending".to_string(),
                    };

                    let dl_id = dl.id.clone();
                    {
                        let mut dlq = self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                        dlq.push_back(dl);
                        // Enforce ring buffer max
                        while dlq.len() > DLQ_MAX_ENTRIES {
                            dlq.pop_front();
                        }
                    }

                    // Auto-retry once for timeout/internal errors
                    if should_auto_retry {
                        // Brief delay before retry
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        // Retry via shared dispatch helper
                        let retry_result = self
                            .retry_dispatch(&tool_name_owned, tool_args_for_dlq)
                            .await;

                        {
                            let mut dlq =
                                self.dead_letters.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(dl) = dlq.iter_mut().find(|dl| dl.id == dl_id) {
                                dl.retry_count = 1;
                                if retry_result.is_ok() {
                                    dl.status = "resolved".to_string();
                                } else {
                                    dl.status = "abandoned".to_string();
                                    if let Err(ref e) = retry_result {
                                        dl.error = format!("{e}");
                                    }
                                }
                            }
                        }

                        if retry_result.is_ok() {
                            // Cache the retry result if applicable
                            if let (Some(key), Ok(ref res)) = (&cache_key, &retry_result) {
                                if let Ok(mut cache) = self.tool_cache.lock() {
                                    cache.insert(
                                        key.clone(),
                                        CachedResult {
                                            result: res.clone(),
                                            created_at: Instant::now(),
                                        },
                                    );
                                }
                            }
                            return retry_result;
                        }
                    }
                }
            }

            // ─── Phantom Tools: store result in cache ────────────────────
            if let (Some(key), Ok(ref res)) = (&cache_key, &result) {
                if let Ok(mut cache) = self.tool_cache.lock() {
                    cache.insert(
                        key.clone(),
                        CachedResult {
                            result: res.clone(),
                            created_at: Instant::now(),
                        },
                    );
                }
            }

            result
        }
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    if let Err(e) = tokio_main(cli) {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn tokio_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Load config from dotenv files (same as before)
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let expand_user_path = |raw: &str| {
        if raw == "~" {
            home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(raw)
        }
    };

    let app_home = std::env::var("TACHI_HOME")
        .or_else(|_| std::env::var("SIGIL_HOME"))
        .map(|v| expand_user_path(&v))
        .unwrap_or_else(|_| home.join(".tachi"));
    let git_root = find_git_root();

    let expand_cli_path = |raw: &PathBuf| expand_user_path(raw.to_string_lossy().as_ref());

    let _ = dotenvy::from_path(home.join(".secrets/master.env"));
    let _ = dotenvy::from_path_override(app_home.join("config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".tachi/config.env"));
    // Backward compatibility with old Sigil paths
    let _ = dotenvy::from_path_override(home.join(".sigil/config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".sigil/config.env"));

    // Project-local dotenv support (non-overriding):
    // - current working directory .env
    // - git root .env (if different from cwd)
    if let Ok(cwd) = std::env::current_dir() {
        let _ = dotenvy::from_path(cwd.join(".env"));
        if let Some(root) = git_root.as_ref() {
            if root != &cwd {
                let _ = dotenvy::from_path(root.join(".env"));
            }
        }
    }

    let gc_enabled = cli
        .gc_enabled
        .or_else(|| parse_env_bool("MEMORY_GC_ENABLED"))
        .unwrap_or(true);
    let gc_initial_delay_secs = cli
        .gc_initial_delay_secs
        .or_else(|| parse_env_u64("MEMORY_GC_INITIAL_DELAY_SECS"))
        .unwrap_or(300);
    let mut gc_interval_secs = cli
        .gc_interval_secs
        .or_else(|| parse_env_u64("MEMORY_GC_INTERVAL_SECS"))
        .unwrap_or(6 * 3600);
    if gc_interval_secs == 0 {
        eprintln!("MEMORY_GC_INTERVAL_SECS/--gc-interval-secs must be >= 1; using 1 second");
        gc_interval_secs = 1;
    }

    // Resolve global DB path
    let global_db_path = if let Some(p) = cli.global_db.as_ref() {
        expand_cli_path(p)
    } else if let Ok(p) = std::env::var("MEMORY_DB_PATH") {
        expand_user_path(&p)
    } else {
        let default_global = app_home.join("global/memory.db");
        // Migration: move legacy DBs into ${TACHI_HOME}/global/memory.db
        let legacy_candidates = vec![
            app_home.join("memory.db"),
            home.join(".sigil/global/memory.db"),
            home.join(".sigil/memory.db"),
        ];
        if !default_global.exists() {
            for legacy in legacy_candidates {
                if legacy.exists() {
                    if let Some(parent) = default_global.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::copy(&legacy, &default_global).await?;
                    eprintln!(
                        "Migrated legacy DB: {} → {}",
                        legacy.display(),
                        default_global.display()
                    );
                    break;
                }
            }
        }
        default_global
    };

    // Resolve project DB path
    let project_db_path = if cli.no_project_db {
        if cli.project_db.is_some() {
            eprintln!("--project-db is ignored because --no-project-db is set");
        }
        None
    } else if let Some(p) = cli.project_db.as_ref() {
        Some(expand_cli_path(p))
    } else if let Some(root) = git_root.as_ref() {
        let project_default = root.join(".tachi/memory.db");
        let project_legacy = root.join(".sigil/memory.db");

        if project_legacy.exists() && !project_default.exists() {
            if let Some(parent) = project_default.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&project_legacy, &project_default).await?;
            eprintln!(
                "Migrated legacy project DB: {} → {}",
                project_legacy.display(),
                project_default.display()
            );
        }

        Some(project_default)
    } else {
        None
    };

    // Ensure parent dirs exist
    if let Some(parent) = global_db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(ref p) = project_db_path {
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let server = MemoryServer::new(global_db_path.clone(), project_db_path.clone())?;

    // Spawn idle connection cleanup task
    {
        let pool = server.pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut conns = pool.connections.lock().unwrap();
                let now = Instant::now();
                let idle_ttl = pool.idle_ttl;
                let stale: Vec<String> = conns
                    .iter()
                    .filter(|(_, c)| now.duration_since(c.last_used) > idle_ttl)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in stale {
                    if let Some(conn) = conns.remove(&key) {
                        eprintln!("Idle cleanup: disconnecting '{}'", key);
                        drop(conn);
                    }
                }
            }
        });
    }

    if gc_enabled {
        eprintln!(
            "Background GC enabled (initial_delay={}s, interval={}s)",
            gc_initial_delay_secs, gc_interval_secs
        );
        let gc_server = server.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(gc_initial_delay_secs)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(gc_interval_secs));
            loop {
                interval.tick().await;
                eprintln!("[gc] Running scheduled garbage collection...");
                match gc_server
                    .with_global_store(|store| store.gc_tables().map_err(|e| format!("{}", e)))
                {
                    Ok(result) => eprintln!("[gc] Global DB: {}", result),
                    Err(e) => eprintln!("[gc] Global DB error: {}", e),
                }
                if gc_server.project_db_path.is_some() {
                    match gc_server
                        .with_project_store(|store| store.gc_tables().map_err(|e| format!("{}", e)))
                    {
                        Ok(result) => eprintln!("[gc] Project DB: {}", result),
                        Err(e) => eprintln!("[gc] Project DB error: {}", e),
                    }
                }
            }
        });
    } else {
        eprintln!("Background GC disabled");
    }

    // Integrity check on global DB
    server
        .with_global_store(|store| {
            match store.quick_check() {
                Ok(true) => eprintln!("Global database integrity: OK"),
                Ok(false) => eprintln!("WARNING: Global database may be corrupted!"),
                Err(e) => eprintln!("WARNING: Could not check global database integrity: {e}"),
            }
            Ok(())
        })
        .map_err(|e| format!("startup check: {e}"))?;

    // Integrity check on project DB
    if project_db_path.is_some() {
        server
            .with_project_store(|store| {
                match store.quick_check() {
                    Ok(true) => eprintln!("Project database integrity: OK"),
                    Ok(false) => eprintln!("WARNING: Project database may be corrupted!"),
                    Err(e) => {
                        eprintln!("WARNING: Could not check project database integrity: {e}")
                    }
                }
                Ok(())
            })
            .map_err(|e| format!("startup check: {e}"))?;
    }

    // Load cached proxy tools from Hub
    {
        let load_proxy_tools = |store: &mut MemoryStore| -> Result<(), String> {
            let mcp_caps = store
                .hub_list(Some("mcp"), true)
                .map_err(|e| format!("hub list: {e}"))?;
            for cap in mcp_caps {
                let def: serde_json::Value =
                    serde_json::from_str(&cap.definition).unwrap_or_default();
                if let Some(tools_json) = def.get("discovered_tools") {
                    if let Ok(tools) =
                        serde_json::from_value::<Vec<rmcp::model::Tool>>(tools_json.clone())
                    {
                        let server_name = cap.id.strip_prefix("mcp:").unwrap_or(&cap.id);
                        let filtered_tools = filter_mcp_tools_by_permissions(&def, tools);
                        server
                            .proxy_tools
                            .lock()
                            .unwrap()
                            .insert(server_name.to_string(), filtered_tools);
                    }
                }
            }
            Ok(())
        };
        let _ = server.with_global_store(load_proxy_tools);
        if server.project_db_path.is_some() {
            let _ = server.with_project_store(load_proxy_tools);
        }
    }
    {
        let load_skill_tools = |store: &mut MemoryStore| -> Result<(), String> {
            let skill_caps = store
                .hub_list(Some("skill"), true)
                .map_err(|e| format!("hub list: {e}"))?;
            for cap in skill_caps {
                let _ = server.register_skill_tool(&cap);
            }
            Ok(())
        };
        let _ = server.with_global_store(load_skill_tools);
        if server.project_db_path.is_some() {
            let _ = server.with_project_store(load_skill_tools);
        }
    }

    if server.pipeline_enabled {
        eprintln!("Pipeline workers: ENABLED (external)");
    } else {
        eprintln!("Pipeline workers: DISABLED (set ENABLE_PIPELINE=true to enable)");
    }

    eprintln!("Starting Tachi MCP Server v{}", env!("CARGO_PKG_VERSION"));
    eprintln!(
        "Transport: {}",
        if cli.daemon {
            format!("HTTP daemon on port {}", cli.port)
        } else {
            "stdio".to_string()
        }
    );
    eprintln!("Global DB: {}", global_db_path.display());
    if let Some(ref p) = project_db_path {
        eprintln!("Project DB: {}", p.display());
    } else {
        eprintln!("Project DB: none (not in a git repository)");
    }
    eprintln!(
        "Vector search: global={}, project={}",
        server.global_vec_available, server.project_vec_available
    );

    if cli.daemon {
        // ── HTTP daemon mode ───────────────────────────────────���─────────
        // TODO: In daemon mode we only use the global DB since there is no
        //       single project context. Per-session project DB routing will
        //       require a session-aware factory that resolves the project path
        //       from a request header or query parameter.

        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };
        use tokio_util::sync::CancellationToken;

        let ct = CancellationToken::new();
        let ct_shutdown = ct.clone();
        let port = cli.port;

        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                cancellation_token: ct.child_token(),
                ..Default::default()
            },
        );

        let router = axum::Router::new().nest_service("/mcp", service);
        let bind_addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

        eprintln!("Tachi daemon listening on http://{bind_addr}");

        tokio::select! {
            result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await }) => {
                if let Err(e) = result {
                    eprintln!("HTTP server error: {e}");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Received SIGINT, shutting down gracefully...");
                ct.cancel();
            }
        }
    } else {
        // ── stdio mode (default) ─────────────────────────────────────────
        let transport = (stdin(), stdout());
        let running = rmcp::service::serve_server(server, transport).await?;

        // Graceful shutdown: wait for either MCP quit or SIGINT/SIGTERM
        tokio::select! {
            quit_reason = running.waiting() => {
                eprintln!("Memory MCP Server stopped: {:?}", quit_reason);
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Received SIGINT, shutting down gracefully...");
            }
        }
    }

    Ok(())
}

/// Stable hash function (FNV-1a). Deterministic across Rust toolchain versions,
/// unlike DefaultHasher which uses SipHash with randomized keys.
fn stable_hash(input: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash)
}

fn parse_env_bool(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => {
            eprintln!("Ignoring invalid {name} value '{raw}' (expected true/false)");
            None
        }
    }
}

fn parse_env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("Ignoring invalid {name} value '{raw}' (expected non-negative integer)");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_test_env() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            std::env::set_var("VOYAGE_API_KEY", "test-voyage-key");
            std::env::set_var("SILICONFLOW_API_KEY", "test-siliconflow-key");
            std::env::set_var("SILICONFLOW_MODEL", "test-model");
            std::env::set_var("SUMMARY_MODEL", "test-summary-model");
        });
    }

    fn make_server() -> MemoryServer {
        ensure_test_env();
        let db_path = std::env::temp_dir().join(format!(
            "memory-server-test-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        MemoryServer::new(db_path, None).expect("failed to create test server")
    }

    fn make_entry(id: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            path: "/".to_string(),
            summary: "".to_string(),
            text: "test memory".to_string(),
            importance: 0.7,
            timestamp: Utc::now().to_rfc3339(),
            category: "fact".to_string(),
            topic: "".to_string(),
            keywords: Vec::new(),
            persons: Vec::new(),
            entities: Vec::new(),
            location: "".to_string(),
            source: "test".to_string(),
            scope: "general".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            metadata: json!({}),
            vector: None,
        }
    }

    fn make_test_tool(name: &str) -> rmcp::model::Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": format!("tool {name}"),
            "inputSchema": {
                "type": "object",
                "additionalProperties": true,
            }
        }))
        .expect("failed to build test tool")
    }

    #[tokio::test]
    async fn proxy_call_blocks_disabled_capability_even_directly() {
        let server = make_server();

        let cap = HubCapability {
            id: "mcp:blocked".to_string(),
            cap_type: "mcp".to_string(),
            name: "blocked".to_string(),
            version: 1,
            description: "test disabled server".to_string(),
            definition: r#"{"transport":"stdio","command":"npx","args":[]}"#.to_string(),
            enabled: false,
            uses: 0,
            successes: 0,
            failures: 0,
            avg_rating: 0.0,
            last_used: None,
            created_at: String::new(),
            updated_at: String::new(),
        };

        server
            .with_global_store(|store| {
                store
                    .hub_register(&cap)
                    .map_err(|e| format!("register failed: {e}"))
            })
            .expect("failed to register capability");

        let err = server
            .proxy_call_internal("blocked", "some_tool", None)
            .await
            .expect_err("disabled MCP capability should be blocked");

        assert!(
            err.to_string().contains("disabled"),
            "expected disabled error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn ghost_subscribe_evicts_least_recent_cursor_when_full() {
        let server = make_server();

        {
            let mut state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..PUBSUB_MAX_CURSORS {
                let agent_id = format!("agent-{i}");
                state.cursors.insert(agent_id.clone(), HashMap::new());
                state.cursor_recency.insert(agent_id, (i as u64) + 1);
            }
            state.cursor_seq = PUBSUB_MAX_CURSORS as u64;
        }

        let params = GhostSubscribeParams {
            agent_id: "agent-new".to_string(),
            topics: vec![],
        };

        let _ = server
            .ghost_subscribe(Parameters(params))
            .await
            .expect("ghost_subscribe should succeed");

        let state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.cursors.len(), PUBSUB_MAX_CURSORS);
        assert!(
            !state.cursors.contains_key("agent-0"),
            "expected least-recent agent to be evicted"
        );
        assert!(state.cursors.contains_key("agent-new"));
        assert!(state.cursor_recency.contains_key("agent-new"));
    }

    #[tokio::test]
    async fn sync_memories_errors_if_agent_state_persist_fails() {
        let server = make_server();

        server
            .with_global_store(|store| {
                store
                    .upsert(&make_entry("sync-1"))
                    .map_err(|e| format!("upsert failed: {e}"))
            })
            .expect("failed to seed memory");

        server
            .with_global_store(|store| {
                store
                    .connection()
                    .execute_batch(
                        r#"
                        DROP TRIGGER IF EXISTS block_agent_known_state_insert;
                        CREATE TRIGGER block_agent_known_state_insert
                        BEFORE INSERT ON agent_known_state
                        BEGIN
                            SELECT RAISE(FAIL, 'blocked by test');
                        END;
                        "#,
                    )
                    .map_err(|e| format!("trigger setup failed: {e}"))
            })
            .expect("failed to install blocking trigger");

        let params = SyncMemoriesParams {
            agent_id: "agent-sync-test".to_string(),
            path_prefix: Some("/".to_string()),
            limit: 10,
        };

        let err = server
            .sync_memories(Parameters(params))
            .await
            .expect_err("sync_memories should fail when state persistence fails");

        assert!(
            err.contains("failed to persist agent state"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn filter_mcp_tools_respects_allow_and_deny_permissions() {
        let def = json!({
            "permissions": {
                "allow": ["echo", "add"],
                "deny": ["add"],
            }
        });

        let filtered = filter_mcp_tools_by_permissions(
            &def,
            vec![
                make_test_tool("echo"),
                make_test_tool("add"),
                make_test_tool("secret"),
            ],
        );

        let names: Vec<String> = filtered
            .iter()
            .map(|tool| tool.name.as_ref().to_string())
            .collect();
        assert_eq!(names, vec!["echo"]);
    }

    #[tokio::test]
    async fn hub_register_discovery_failure_disables_capability_and_clears_proxy_tools() {
        let server = make_server();
        let params = HubRegisterParams {
            id: "mcp:discovery-fails".to_string(),
            cap_type: "mcp".to_string(),
            name: "discovery-fails".to_string(),
            description: "test discovery failure".to_string(),
            definition: json!({
                "transport": "stdio",
                "command": "/usr/bin/true",
                "args": [],
            })
            .to_string(),
            version: 1,
            scope: "global".to_string(),
        };

        let response = server
            .hub_register(Parameters(params))
            .await
            .expect("hub_register should return response");
        let data: serde_json::Value =
            serde_json::from_str(&response).expect("hub_register response should be JSON");

        assert_eq!(data.get("enabled"), Some(&json!(false)));
        assert!(
            data.get("discovery_error")
                .and_then(|v| v.as_str())
                .is_some(),
            "expected discovery_error in response: {data}"
        );

        let cap = server
            .get_capability("mcp:discovery-fails")
            .expect("capability should be persisted");
        assert!(
            !cap.enabled,
            "capability should be disabled after discovery failure"
        );

        let def: serde_json::Value =
            serde_json::from_str(&cap.definition).expect("stored definition should be valid JSON");
        assert_eq!(def.get("discovery_status"), Some(&json!("failed")));
        assert!(
            def.get("last_discovery_error")
                .and_then(|v| v.as_str())
                .is_some(),
            "stored definition should include last_discovery_error"
        );

        let proxy_tools = server.proxy_tools.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !proxy_tools.contains_key("discovery-fails"),
            "failed capability should not leave proxy tools cached"
        );
    }

    #[test]
    fn resolve_mcp_tool_exposure_supports_definition_overrides() {
        let flatten = resolve_mcp_tool_exposure(
            &json!({"tool_exposure": "flatten"}),
            McpToolExposureMode::Gateway,
        );
        let gateway = resolve_mcp_tool_exposure(
            &json!({"tool_exposure": "gateway"}),
            McpToolExposureMode::Flatten,
        );
        let expose_false = resolve_mcp_tool_exposure(
            &json!({"expose_tools": false}),
            McpToolExposureMode::Flatten,
        );
        let fallback_default = resolve_mcp_tool_exposure(&json!({}), McpToolExposureMode::Gateway);

        assert_eq!(flatten, McpToolExposureMode::Flatten);
        assert_eq!(gateway, McpToolExposureMode::Gateway);
        assert_eq!(expose_false, McpToolExposureMode::Gateway);
        assert_eq!(fallback_default, McpToolExposureMode::Gateway);
    }

    #[tokio::test]
    async fn retry_dispatch_blocks_direct_proxy_tool_when_gateway_mode() {
        let server = make_server();

        let cap = HubCapability {
            id: "mcp:gateway-only".to_string(),
            cap_type: "mcp".to_string(),
            name: "gateway-only".to_string(),
            version: 1,
            description: "gateway mode mcp".to_string(),
            definition: json!({
                "transport": "stdio",
                "command": "npx",
                "args": ["-y", "dummy-mcp"],
                "tool_exposure": "gateway",
            })
            .to_string(),
            enabled: true,
            uses: 0,
            successes: 0,
            failures: 0,
            avg_rating: 0.0,
            last_used: None,
            created_at: String::new(),
            updated_at: String::new(),
        };

        server
            .with_global_store(|store| {
                store
                    .hub_register(&cap)
                    .map_err(|e| format!("register failed: {e}"))
            })
            .expect("failed to register gateway capability");

        let err = server
            .retry_dispatch(
                "gateway-only__echo",
                Some(serde_json::Map::from_iter([(
                    "text".to_string(),
                    json!("hello"),
                )])),
            )
            .await
            .expect_err("gateway mode should block direct proxy tool names");

        assert!(
            err.to_string().contains("tool_exposure=gateway"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn post_card_check_inbox_and_update_roundtrip() {
        std::env::set_var("KANBAN_CLASSIFY_ENABLED", "false");
        let server = make_server();

        let posted = server
            .post_card(Parameters(PostCardParams {
                from_agent: "hapi".to_string(),
                to_agent: "iris".to_string(),
                title: "Need review".to_string(),
                body: "Please review PR #42".to_string(),
                priority: "high".to_string(),
                card_type: "request".to_string(),
                thread_id: Some("thread-42".to_string()),
            }))
            .await
            .expect("post_card should succeed");
        let posted_json: serde_json::Value =
            serde_json::from_str(&posted).expect("post_card response should be JSON");
        let card_id = posted_json["card_id"]
            .as_str()
            .expect("post_card should return card_id")
            .to_string();

        let inbox = server
            .check_inbox(Parameters(CheckInboxParams {
                agent_id: "iris".to_string(),
                status_filter: Some("open".to_string()),
                since: None,
                include_broadcast: true,
                limit: 20,
            }))
            .await
            .expect("check_inbox should succeed");
        let inbox_json: serde_json::Value =
            serde_json::from_str(&inbox).expect("check_inbox response should be JSON");
        let cards = inbox_json["cards"]
            .as_array()
            .expect("check_inbox should return cards array");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["id"], json!(card_id));
        assert_eq!(cards[0]["status"], json!("open"));

        let updated = server
            .update_card(Parameters(UpdateCardParams {
                card_id: card_id.clone(),
                new_status: "acknowledged".to_string(),
                response_text: Some("Got it".to_string()),
            }))
            .await
            .expect("update_card should succeed");
        let updated_json: serde_json::Value =
            serde_json::from_str(&updated).expect("update_card response should be JSON");
        assert_eq!(updated_json["updated"], json!(true));

        let inbox_after = server
            .check_inbox(Parameters(CheckInboxParams {
                agent_id: "iris".to_string(),
                status_filter: Some("acknowledged".to_string()),
                since: None,
                include_broadcast: true,
                limit: 20,
            }))
            .await
            .expect("check_inbox after update should succeed");
        let inbox_after_json: serde_json::Value =
            serde_json::from_str(&inbox_after).expect("check_inbox after update should be JSON");
        let cards_after = inbox_after_json["cards"]
            .as_array()
            .expect("cards_after should be array");
        assert_eq!(cards_after.len(), 1);
        assert_eq!(cards_after[0]["status"], json!("acknowledged"));
        assert!(
            cards_after[0]["body"]
                .as_str()
                .unwrap_or_default()
                .contains("Got it"),
            "response text should be appended to body"
        );
    }

    #[tokio::test]
    async fn check_inbox_respects_broadcast_toggle() {
        std::env::set_var("KANBAN_CLASSIFY_ENABLED", "false");
        let server = make_server();

        server
            .post_card(Parameters(PostCardParams {
                from_agent: "aegis".to_string(),
                to_agent: "*".to_string(),
                title: "Fleet alert".to_string(),
                body: "CI pipeline blocked".to_string(),
                priority: "critical".to_string(),
                card_type: "alert".to_string(),
                thread_id: None,
            }))
            .await
            .expect("post_card broadcast should succeed");

        let no_broadcast = server
            .check_inbox(Parameters(CheckInboxParams {
                agent_id: "iris".to_string(),
                status_filter: None,
                since: None,
                include_broadcast: false,
                limit: 10,
            }))
            .await
            .expect("check_inbox without broadcast should succeed");
        let no_broadcast_json: serde_json::Value =
            serde_json::from_str(&no_broadcast).expect("check_inbox response should be JSON");
        assert_eq!(no_broadcast_json["count"], json!(0));

        let with_broadcast = server
            .check_inbox(Parameters(CheckInboxParams {
                agent_id: "iris".to_string(),
                status_filter: None,
                since: None,
                include_broadcast: true,
                limit: 10,
            }))
            .await
            .expect("check_inbox with broadcast should succeed");
        let with_broadcast_json: serde_json::Value =
            serde_json::from_str(&with_broadcast).expect("check_inbox response should be JSON");
        assert_eq!(with_broadcast_json["count"], json!(1));
    }
}
