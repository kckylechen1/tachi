// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod bootstrap;
mod dlq_ops;
mod ghost_ops;
mod graph_state_ops;
mod hub_helpers;
mod hub_ops;
mod kanban;
mod llm;
mod mcp_connection;
mod mcp_proxy;
mod memory_ops;
mod memory_search_ops;
mod pipeline_ops;
mod prompts;
mod sandbox_ops;
mod server_methods;
mod server_handler;
mod shared_defs;
mod skill_chain_ops;
mod tool_params;
mod utils;

use crate::hub_helpers::{build_skill_tool_from_cap, make_text_tool_result};
use crate::ghost_ops::{handle_ghost_publish, handle_ghost_subscribe, handle_ghost_topics};
use crate::graph_state_ops::{
    handle_add_edge, handle_get_edges, handle_get_state, handle_set_state,
};
use crate::hub_ops::{
    handle_hub_call, handle_hub_disconnect, handle_hub_discover, handle_hub_feedback,
    handle_hub_get, handle_hub_register, handle_hub_set_enabled, handle_hub_stats, handle_run_skill,
    handle_tachi_audit_log,
};
use crate::dlq_ops::{handle_dlq_list, handle_dlq_retry};
use crate::kanban::{
    handle_check_inbox, handle_post_card, handle_update_card, CheckInboxParams, PostCardParams,
    UpdateCardParams,
};
use crate::mcp_proxy::{
    append_warning, clear_mcp_discovery_metadata, filter_mcp_tools_by_permissions,
    resolve_mcp_tool_exposure, set_mcp_discovery_failure, set_mcp_discovery_success,
    McpToolExposureMode,
};
use crate::memory_ops::{
    handle_archive_memory, handle_delete_memory, handle_get_memory, handle_list_memories,
    handle_memory_gc, handle_memory_stats,
};
use crate::memory_search_ops::{
    handle_find_similar_memory, handle_save_memory, handle_search_memory,
};
use crate::pipeline_ops::{
    handle_extract_facts, handle_get_pipeline_status, handle_ingest_event, handle_sync_memories,
};
use crate::sandbox_ops::{handle_sandbox_check, handle_sandbox_set_rule};
use crate::shared_defs::{
    categorize_error, slim_entry, slim_l0_rule, slim_search_result, DeadLetter, DLQ_MAX_ENTRIES,
    DLQ_TTL_SECS,
};
use crate::skill_chain_ops::handle_chain_skills;
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
        handle_save_memory(self, params).await
    }

    #[tool(
        description = "Search memory entries using hybrid search (vector + FTS + symbolic). Returns ranked results with scores."
    )]
    async fn search_memory(
        &self,
        Parameters(params): Parameters<SearchMemoryParams>,
    ) -> Result<String, String> {
        handle_search_memory(self, params).await
    }

    #[tool(
        description = "Find memory entries similar to a provided vector. Uses vector similarity only (no FTS/symbolic/decay weighting)."
    )]
    async fn find_similar_memory(
        &self,
        Parameters(params): Parameters<FindSimilarMemoryParams>,
    ) -> Result<String, String> {
        handle_find_similar_memory(self, params).await
    }

    #[tool(description = "Get a single memory entry by ID.")]
    async fn get_memory(
        &self,
        Parameters(params): Parameters<GetMemoryParams>,
    ) -> Result<String, String> {
        handle_get_memory(self, params).await
    }

    #[tool(description = "List memory entries under a path prefix.")]
    async fn list_memories(
        &self,
        Parameters(params): Parameters<ListMemoriesParams>,
    ) -> Result<String, String> {
        handle_list_memories(self, params).await
    }

    #[tool(description = "Get aggregate statistics about the memory store.")]
    async fn memory_stats(&self) -> Result<String, String> {
        handle_memory_stats(self).await
    }

    #[tool(
        description = "Delete a memory entry permanently. Removes from main table, FTS, vectors, graph edges, and access history."
    )]
    async fn delete_memory(
        &self,
        Parameters(params): Parameters<DeleteMemoryParams>,
    ) -> Result<String, String> {
        handle_delete_memory(self, params).await
    }

    #[tool(
        description = "Archive a memory entry (soft-delete, set archived=1). Entry is hidden from default searches but can be retrieved with include_archived=true."
    )]
    async fn archive_memory(
        &self,
        Parameters(params): Parameters<ArchiveMemoryParams>,
    ) -> Result<String, String> {
        handle_archive_memory(self, params).await
    }

    #[tool(
        description = "Run garbage collection on growing tables. Prunes old access_history (keep latest 256 per memory), processed_events (30d), audit_log (30d + 100k cap), and agent_known_state (90d)."
    )]
    async fn memory_gc(&self) -> Result<String, String> {
        handle_memory_gc(self).await
    }

    #[tool(description = "Enable or disable a Hub capability by ID.")]
    async fn hub_set_enabled(
        &self,
        Parameters(params): Parameters<HubSetEnabledParams>,
    ) -> Result<String, String> {
        handle_hub_set_enabled(self, params).await
    }

    #[tool(
        description = "Add or update an edge in the memory graph. Edges represent causal, temporal, or entity relationships between memories."
    )]
    async fn add_edge(
        &self,
        Parameters(params): Parameters<AddEdgeParams>,
    ) -> Result<String, String> {
        handle_add_edge(self, params).await
    }

    #[tool(
        description = "Get edges connected to a memory entry. Returns causal, temporal, and entity relationship edges."
    )]
    async fn get_edges(
        &self,
        Parameters(params): Parameters<GetEdgesParams>,
    ) -> Result<String, String> {
        handle_get_edges(self, params).await
    }

    #[tool(description = "Set a key-value pair in server state (stored in hard_state table).")]
    async fn set_state(
        &self,
        Parameters(params): Parameters<SetStateParams>,
    ) -> Result<String, String> {
        handle_set_state(self, params).await
    }

    #[tool(description = "Get a value from server state by key.")]
    async fn get_state(
        &self,
        Parameters(params): Parameters<GetStateParams>,
    ) -> Result<String, String> {
        handle_get_state(self, params).await
    }

    #[tool(description = "Extract structured facts from text using LLM and save to memory.")]
    async fn extract_facts(
        &self,
        Parameters(params): Parameters<ExtractFactsParams>,
    ) -> Result<String, String> {
        handle_extract_facts(self, params).await
    }

    #[tool(description = "Ingest a conversation event and extract facts from messages.")]
    async fn ingest_event(
        &self,
        Parameters(params): Parameters<IngestEventParams>,
    ) -> Result<String, String> {
        handle_ingest_event(self, params).await
    }

    #[tool(description = "Get pipeline status and statistics.")]
    async fn get_pipeline_status(&self) -> Result<String, String> {
        handle_get_pipeline_status(self).await
    }

    #[tool(
        description = "Get only new or changed memories since last sync for this agent. Returns incremental diff to save tokens. Use agent_id to identify your agent uniquely."
    )]
    async fn sync_memories(
        &self,
        Parameters(params): Parameters<SyncMemoriesParams>,
    ) -> Result<String, String> {
        handle_sync_memories(self, params).await
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
        handle_hub_register(self, params).await
    }

    #[tool(
        description = "Discover available capabilities (skills, plugins, MCP servers) in the Hub."
    )]
    async fn hub_discover(
        &self,
        Parameters(params): Parameters<HubDiscoverParams>,
    ) -> Result<String, String> {
        handle_hub_discover(self, params).await
    }

    #[tool(description = "Get a specific capability from the Hub by ID.")]
    async fn hub_get(
        &self,
        Parameters(params): Parameters<HubGetParams>,
    ) -> Result<String, String> {
        handle_hub_get(self, params).await
    }

    #[tool(description = "Record feedback for a Hub capability invocation.")]
    async fn hub_feedback(
        &self,
        Parameters(params): Parameters<HubFeedbackParams>,
    ) -> Result<String, String> {
        handle_hub_feedback(self, params).await
    }

    #[tool(description = "Get Hub capability statistics and metrics.")]
    async fn hub_stats(&self) -> Result<String, String> {
        handle_hub_stats(self).await
    }

    #[tool(
        description = "Execute a registered Skill from the Hub using the internal LLM pipeline."
    )]
    async fn run_skill(
        &self,
        Parameters(params): Parameters<RunSkillParams>,
    ) -> Result<String, String> {
        handle_run_skill(self, params).await
    }

    #[tool(description = "View audit log of proxy tool calls through the Hub.")]
    async fn tachi_audit_log(
        &self,
        Parameters(params): Parameters<AuditLogParams>,
    ) -> Result<String, String> {
        handle_tachi_audit_log(self, params).await
    }

    #[tool(
        description = "Call a tool on a registered MCP server through the Hub using the shared connection pool."
    )]
    async fn hub_call(
        &self,
        Parameters(params): Parameters<HubCallParams>,
    ) -> Result<String, String> {
        handle_hub_call(self, params).await
    }

    #[tool(
        description = "Disconnect a cached MCP server connection from the pool. Forces a fresh reconnect (with updated env/config) on next hub_call."
    )]
    async fn hub_disconnect(
        &self,
        Parameters(params): Parameters<HubDisconnectParams>,
    ) -> Result<String, String> {
        handle_hub_disconnect(self, params).await
    }

    // ─── Ghost Whispers (Inter-Agent Pub/Sub) ────────────────────────────────

    #[tool(
        description = "Publish a message to a Ghost Whispers topic. Other agents can poll for new messages via ghost_subscribe."
    )]
    async fn ghost_publish(
        &self,
        Parameters(params): Parameters<GhostPublishParams>,
    ) -> Result<String, String> {
        handle_ghost_publish(self, params).await
    }

    #[tool(
        description = "Subscribe to Ghost Whispers topics and get new messages since last poll. Advances the cursor so the same messages are not returned again."
    )]
    async fn ghost_subscribe(
        &self,
        Parameters(params): Parameters<GhostSubscribeParams>,
    ) -> Result<String, String> {
        handle_ghost_subscribe(self, params).await
    }

    #[tool(
        description = "List active Ghost Whispers topics with message counts and last message time."
    )]
    async fn ghost_topics(&self) -> Result<String, String> {
        handle_ghost_topics(self).await
    }

    // ─── Skill Chaining (Unix Pipe-Style Composition) ────────────────────────

    #[tool(
        description = "Execute a chain of skills in sequence (Unix pipe style). Output of each skill feeds as input to the next."
    )]
    async fn chain_skills(
        &self,
        Parameters(params): Parameters<ChainSkillsParams>,
    ) -> Result<String, String> {
        handle_chain_skills(self, params).await
    }

    // ─── Dead Letter Queue Tools ──────────────────────────────────────────────

    #[tool(
        description = "List dead letter queue entries (failed tool calls). Filter by status: pending, retrying, resolved, abandoned."
    )]
    async fn dlq_list(
        &self,
        Parameters(params): Parameters<DlqListParams>,
    ) -> Result<String, String> {
        handle_dlq_list(self, params).await
    }

    #[tool(
        description = "Manually retry a dead letter queue entry by its ID. Re-dispatches the failed tool call."
    )]
    async fn dlq_retry(
        &self,
        Parameters(params): Parameters<DlqRetryParams>,
    ) -> Result<String, String> {
        handle_dlq_retry(self, params).await
    }

    // ─── Semantic Sandboxing Tools ───────────────────────────────────────────

    #[tool(
        description = "Set a sandbox access rule for an agent role + path pattern. Controls which memories a role can access. Access levels: read, write, deny."
    )]
    async fn sandbox_set_rule(
        &self,
        Parameters(params): Parameters<SandboxSetRuleParams>,
    ) -> Result<String, String> {
        handle_sandbox_set_rule(self, params).await
    }

    #[tool(
        description = "Check if an agent role can access a given path for a specific operation. Advisory mode — not enforced in search_memory yet (TODO: future enforcement integration)."
    )]
    async fn sandbox_check(
        &self,
        Parameters(params): Parameters<SandboxCheckParams>,
    ) -> Result<String, String> {
        handle_sandbox_check(self, params).await
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
