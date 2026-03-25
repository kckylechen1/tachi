// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod bootstrap;
mod hub_helpers;
mod kanban;
mod llm;
mod mcp_connection;
mod mcp_proxy;
mod prompts;
mod server_handler;
mod tool_params;
mod utils;

use crate::hub_helpers::{build_skill_tool_from_cap, make_text_tool_result};
use crate::kanban::{
    handle_check_inbox, handle_post_card, handle_update_card, CheckInboxParams, PostCardParams,
    UpdateCardParams,
};
use crate::mcp_proxy::{
    append_warning, clear_mcp_discovery_metadata, filter_mcp_tools_by_permissions,
    resolve_mcp_tool_exposure, set_mcp_discovery_failure, set_mcp_discovery_success,
    McpToolExposureMode,
};
use crate::tool_params::*;
use crate::utils::{
    find_git_root, is_active_global_rule, is_trusted_command, parse_env_bool, parse_env_u64,
    stable_hash, value_to_template_text,
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

// ─── Tool Parameter Types ───────────────────────────────────────────────────────
//
// Note: dead_code warnings are expected here because the #[tool] macro
// generates code that uses these types through macro expansion.

// Parameter and tool schema definitions moved to `tool_params.rs`.

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
        handle_post_card(self, params).await
    }

    #[tool(description = "Check kanban inbox for a target agent.")]
    async fn check_inbox(
        &self,
        Parameters(params): Parameters<CheckInboxParams>,
    ) -> Result<String, String> {
        handle_check_inbox(self, params).await
    }

    #[tool(description = "Update status of a kanban card.")]
    async fn update_card(
        &self,
        Parameters(params): Parameters<UpdateCardParams>,
    ) -> Result<String, String> {
        handle_update_card(self, params).await
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

// ─── Main ────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    if let Err(e) = bootstrap::run(cli) {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;
