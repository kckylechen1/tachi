// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod bootstrap;
mod clawdoctor;
mod dlq_ops;
mod foundry_ops;
mod foundry_runtime_ops;
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
mod pack_ops;
mod pipeline_ops;
mod project_db_ops;
mod prompts;
mod provenance;
mod sandbox_ops;
mod server_handler;
mod server_methods;
mod shared_defs;
mod skill_chain_ops;
mod tool_params;
mod utils;
mod vault_crypto;
mod vault_ops;

use crate::dlq_ops::{handle_dlq_list, handle_dlq_retry};
use crate::foundry_ops::handle_synthesize_agent_evolution;
use crate::foundry_runtime_ops::{handle_capture_session, handle_recall_context};
use crate::ghost_ops::{
    handle_ghost_ack, handle_ghost_promote, handle_ghost_publish, handle_ghost_reflect,
    handle_ghost_subscribe, handle_ghost_topics,
};
use crate::graph_state_ops::{
    handle_add_edge, handle_get_edges, handle_get_state, handle_set_state,
};
use crate::hub_helpers::{
    build_skill_tool_from_cap, capability_callable, capability_visibility_for_cap,
    make_text_tool_result, review_status_allows_call, should_expose_mcp_tools,
    should_expose_skill_tool, CapabilityVisibility,
};
use crate::hub_ops::{
    handle_export_skills, handle_hub_call, handle_hub_disconnect, handle_hub_discover,
    handle_hub_feedback, handle_hub_get, handle_hub_register, handle_hub_review,
    handle_hub_set_active_version, handle_hub_set_enabled, handle_hub_stats, handle_run_skill,
    handle_skill_evolve, handle_tachi_audit_log, handle_vc_bind, handle_vc_list,
    handle_vc_register, handle_vc_resolve,
};
use crate::kanban::{
    gc_expired_kanban_cards, handle_check_inbox, handle_post_card, handle_update_card,
    CheckInboxParams, PostCardParams, UpdateCardParams, DEFAULT_KANBAN_GC_MAX_AGE_DAYS,
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
use crate::pack_ops::{
    handle_pack_get, handle_pack_list, handle_pack_project, handle_pack_register,
    handle_pack_remove, handle_projection_list,
};
use crate::pipeline_ops::{
    handle_extract_facts, handle_get_pipeline_status, handle_ingest_event, handle_sync_memories,
};
use crate::project_db_ops::handle_tachi_init_project_db;
use crate::sandbox_ops::{
    handle_sandbox_check, handle_sandbox_exec_audit, handle_sandbox_get_policy,
    handle_sandbox_list_policies, handle_sandbox_set_policy, handle_sandbox_set_rule,
};
use crate::shared_defs::{
    categorize_error, slim_entry, slim_l0_rule, slim_search_result, DeadLetter, DLQ_MAX_ENTRIES,
    DLQ_TTL_SECS,
};
use crate::skill_chain_ops::handle_chain_skills;
use crate::tool_params::*;
use crate::utils::{
    find_git_root, is_active_global_rule, is_trusted_command, lock_or_recover, parse_env_bool,
    parse_env_u64, read_or_recover, sanitize_safe_path_name, stable_hash, value_to_template_text,
    write_or_recover,
};
use crate::vault_ops::{
    handle_vault_get, handle_vault_init, handle_vault_list, handle_vault_lock, handle_vault_remove,
    handle_vault_set, handle_vault_setup_rotation, handle_vault_status, handle_vault_unlock,
    VaultGetParams, VaultInitParams, VaultListParams, VaultRemoveParams, VaultSetParams,
    VaultSetupRotationParams, VaultUnlockParams,
};

use chrono::Utc;
use clap::{Parser, Subcommand};
use memory_core::{
    HubCapability, HybridWeights, MemoryEntry, MemoryStore, SearchOptions, VirtualCapabilityBinding,
};
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
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};
use tokio::io::{stdin, stdout};
use tokio::sync::mpsc;

// ─── Enrichment Batcher ──────────────────────────────────────────────────────

/// An item queued for background embedding + summary enrichment.
#[derive(Debug, Clone)]
struct EnrichmentItem {
    id: String,
    text: String,
    needs_embedding: bool,
    needs_summary: bool,
    target_db: DbScope,
    named_project: Option<String>,
    revision: i64,
}

/// Batch enrichment queue configuration.
const ENRICH_BATCH_MAX: usize = 32;
const ENRICH_FLUSH_INTERVAL_MS: u64 = 500;

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

    /// Enable clawdoctor: periodic OpenClaw health check + auto-restart
    /// (overrides CLAWDOCTOR_ENABLED)
    #[arg(long)]
    clawdoctor: Option<bool>,

    /// OpenClaw gateway URL for clawdoctor health checks
    /// (overrides CLAWDOCTOR_URL, default: http://127.0.0.1:18789)
    #[arg(long)]
    clawdoctor_url: Option<String>,

    /// Clawdoctor check interval in seconds (overrides CLAWDOCTOR_INTERVAL_SECS, default: 300)
    #[arg(long)]
    clawdoctor_interval_secs: Option<u64>,

    /// Consecutive failures before clawdoctor triggers a restart
    /// (overrides CLAWDOCTOR_FAIL_THRESHOLD, default: 3)
    #[arg(long)]
    clawdoctor_fail_threshold: Option<u32>,

    /// CLI command (defaults to `serve` when omitted)
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Start MCP Server (default when no subcommand is provided)
    Serve,
    /// Search memories
    Search {
        query: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value_t = 5)]
        top_k: usize,
    },
    /// Save a memory
    Save {
        text: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        importance: Option<f64>,
    },
    /// Show database statistics
    Stats,
    /// Run garbage collection
    Gc,
    /// Hub management
    Hub {
        #[command(subcommand)]
        action: HubAction,
    },
    /// Backfill missing vector embeddings using Voyage API
    BackfillVectors {
        /// Target DB path (defaults to global DB)
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
        /// Batch size for Voyage API calls (max 128)
        #[arg(long, default_value_t = 64)]
        batch_size: usize,
        /// Only count missing entries, don't embed
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum HubAction {
    List {
        #[arg(long)]
        cap_type: Option<String>,
    },
    Register {
        id: String,
        #[arg(long)]
        cap_type: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        definition: String,
        #[arg(long)]
        description: Option<String>,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Stats,
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

// ─── Rate Limiter Constants ──────────────────────────────────────────────────
/// Default requests-per-minute limit per session (0 = unlimited)
const DEFAULT_RATE_LIMIT_RPM: u64 = 0;
/// Default max identical (tool+args) calls within the burst window (0 = unlimited)
const DEFAULT_RATE_LIMIT_BURST: u64 = 8;
/// Burst detection window
const RATE_LIMIT_BURST_WINDOW: Duration = Duration::from_secs(60);

/// Tools whose results can be cached (read-only, no side effects)
const CACHEABLE_TOOLS: &[&str] = &[
    "search_memory",
    "cyberbrain_search",
    "find_similar_memory",
    "get_memory",
    "list_memories",
    "memory_stats",
    "get_state",
    "hub_discover",
    "hub_get",
    "hub_stats",
    "vc_list",
    "vc_resolve",
    "get_pipeline_status",
];

/// Tools that invalidate the cache (write operations)
const CACHE_INVALIDATING_TOOLS: &[&str] = &[
    "save_memory",
    "cyberbrain_write",
    "extract_facts",
    "ingest_event",
    "set_state",
    "hub_register",
    "hub_review",
    "section9_review",
    "hub_set_active_version",
    "hub_export_skills",
    "skill_evolve",
    "capture_session",
    "synthesize_agent_evolution",
    "vc_register",
    "vc_bind",
    "hub_feedback",
    "sandbox_set_rule",
    "sandbox_set_policy",
    "shell_set_policy",
    "tachi_init_project_db",
    "ghost_reflect",
    "ghost_promote",
    "pack_register",
    "pack_remove",
    "pack_project",
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
struct ProjectDbState {
    store: Arc<StdMutex<MemoryStore>>,
    rw_gate: Arc<StdRwLock<()>>,
    db_path: Arc<PathBuf>,
    vec_available: bool,
}

/// Agent profile registered via `agent_register`. Stored per-session (in-memory).
#[derive(Debug, Clone, serde::Serialize)]
struct AgentProfile {
    agent_id: String,
    display_name: String,
    capabilities: Vec<String>,
    tool_filter: Option<Vec<String>>,
    rate_limit_rpm: Option<u64>,
    rate_limit_burst: Option<u64>,
    registered_at: String,
}

/// Cross-agent handoff memo — left by one agent for the next.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct HandoffMemo {
    id: String,
    from_agent: String,
    target_agent: Option<String>,
    summary: String,
    next_steps: Vec<String>,
    context: Option<serde_json::Value>,
    created_at: String,
    acknowledged: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
struct MemoryServer {
    global_store: Arc<StdMutex<MemoryStore>>,
    project_store: Option<Arc<StdMutex<MemoryStore>>>,
    /// Read/write gate for global DB access. Read operations share the lock,
    /// write operations take exclusive lock.
    global_rw_gate: Arc<StdRwLock<()>>,
    /// Read/write gate for project DB access.
    project_rw_gate: Option<Arc<StdRwLock<()>>>,
    global_db_path: Arc<PathBuf>,
    project_db_path: Option<Arc<PathBuf>>,
    global_vec_available: bool,
    project_vec_available: bool,
    /// Hot-swappable project DB state — allows `tachi_init_project_db` to activate
    /// a project database on a running daemon without restart.
    hot_project_db: Arc<StdRwLock<Option<ProjectDbState>>>,
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
    // ─── Enrichment Batcher ──────────────────────────────────────────────────
    enrich_tx: mpsc::UnboundedSender<EnrichmentItem>,
    // ─── Vault (Encrypted Secret Storage) ────────────────────────────────────
    vault_key: Arc<StdRwLock<Option<[u8; 32]>>>,
    vault_unlock_time: Arc<StdRwLock<Option<Instant>>>,
    vault_failed_attempts: Arc<StdMutex<(u32, Option<Instant>)>>,
    vault_auto_lock_after_secs: u64,
    // ─── Rate Limiter ────────────────────────────────────────────────────────
    /// Sliding window: tool call timestamps per session. Key = session_id (or "default").
    rate_limit_windows: Arc<StdMutex<HashMap<String, VecDeque<Instant>>>>,
    /// Burst detection: (tool_name + args_hash) → timestamps
    rate_limit_bursts: Arc<StdMutex<HashMap<String, VecDeque<Instant>>>>,
    /// Configured RPM limit (0 = unlimited)
    rate_limit_rpm: u64,
    /// Configured burst limit (0 = unlimited)
    rate_limit_burst: u64,
    // ─── Agent Profile ───────────────────────────────────────────────────────
    /// Per-session agent profile (set via agent_register tool).
    agent_profile: Arc<StdRwLock<Option<AgentProfile>>>,
    // ─── Cross-Agent Handoff ─────────────────────────────────────────────────
    /// Pending handoff memos from previous agent sessions.
    handoff_memos: Arc<StdMutex<Vec<HandoffMemo>>>,
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
        let global_db_str = global_db_path.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Global DB path contains invalid UTF-8: {}",
                    global_db_path.display()
                ),
            )
        })?;
        let global_store = MemoryStore::open(global_db_str)?;
        let global_vec_available = global_store.vec_available;

        let (project_store, project_rw_gate, project_db_path, project_vec_available) =
            if let Some(ref p) = project_db_path {
                let project_db_str = p.to_str().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("Project DB path contains invalid UTF-8: {}", p.display()),
                    )
                })?;
                let store = MemoryStore::open(project_db_str)?;
                let v = store.vec_available;
                (
                    Some(Arc::new(StdMutex::new(store))),
                    Some(Arc::new(StdRwLock::new(()))),
                    Some(Arc::new(p.clone())),
                    v,
                )
            } else {
                (None, None, None, false)
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

        let (enrich_tx, enrich_rx) = mpsc::unbounded_channel::<EnrichmentItem>();

        // Build hot-swap state before moving project_store into the struct
        let hot_project_db = Arc::new(StdRwLock::new(project_store.as_ref().map(|s| {
            ProjectDbState {
                store: Arc::clone(s),
                rw_gate: project_rw_gate
                    .clone()
                    .expect("project_rw_gate must exist if project_store exists"),
                db_path: project_db_path
                    .clone()
                    .expect("project_db_path must exist if project_store exists"),
                vec_available: project_vec_available,
            }
        })));

        let server = Self {
            global_store: Arc::new(StdMutex::new(global_store)),
            project_store,
            global_rw_gate: Arc::new(StdRwLock::new(())),
            project_rw_gate: project_rw_gate.clone(),
            global_db_path: Arc::new(global_db_path),
            project_db_path: project_db_path.clone(),
            global_vec_available,
            project_vec_available,
            hot_project_db,
            llm: llm.clone(),
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
            enrich_tx,
            vault_key: Arc::new(StdRwLock::new(None)),
            vault_unlock_time: Arc::new(StdRwLock::new(None)),
            vault_failed_attempts: Arc::new(StdMutex::new((0, None))),
            vault_auto_lock_after_secs: 1800,
            rate_limit_windows: Arc::new(StdMutex::new(HashMap::new())),
            rate_limit_bursts: Arc::new(StdMutex::new(HashMap::new())),
            rate_limit_rpm: parse_env_u64("RATE_LIMIT_RPM").unwrap_or(DEFAULT_RATE_LIMIT_RPM),
            rate_limit_burst: parse_env_u64("RATE_LIMIT_BURST").unwrap_or(DEFAULT_RATE_LIMIT_BURST),
            agent_profile: Arc::new(StdRwLock::new(None)),
            handoff_memos: Arc::new(StdMutex::new(Vec::new())),
        };

        // Spawn the enrichment batcher worker
        {
            let batcher_server = server.clone();
            tokio::spawn(Self::run_enrichment_batcher(batcher_server, enrich_rx));
        }

        Ok(server)
    }

    /// Background worker that batches enrichment requests (embedding + summary).
    /// Flushes every ENRICH_FLUSH_INTERVAL_MS or when ENRICH_BATCH_MAX items accumulate.
    async fn run_enrichment_batcher(
        server: MemoryServer,
        mut rx: mpsc::UnboundedReceiver<EnrichmentItem>,
    ) {
        let mut batch: Vec<EnrichmentItem> = Vec::with_capacity(ENRICH_BATCH_MAX);
        let flush_interval = Duration::from_millis(ENRICH_FLUSH_INTERVAL_MS);

        loop {
            // Wait for first item or channel close
            let item = if batch.is_empty() {
                match rx.recv().await {
                    Some(item) => Some(item),
                    None => break, // channel closed
                }
            } else {
                None
            };

            if let Some(item) = item {
                batch.push(item);
            }

            // Drain more items until batch is full or timeout expires
            let deadline = tokio::time::Instant::now() + flush_interval;
            while batch.len() < ENRICH_BATCH_MAX {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(item)) => batch.push(item),
                    Ok(None) => {
                        // Channel closed; flush remaining and exit
                        if !batch.is_empty() {
                            server.flush_enrichment_batch(&mut batch).await;
                        }
                        return;
                    }
                    Err(_timeout) => break, // timer expired, flush what we have
                }
            }

            if !batch.is_empty() {
                server.flush_enrichment_batch(&mut batch).await;
            }
        }

        eprintln!("[enrichment-batcher] channel closed, worker exiting");
    }

    /// Flush a batch: batch-embed all texts needing embedding, then update DB.
    async fn flush_enrichment_batch(&self, batch: &mut Vec<EnrichmentItem>) {
        let items: Vec<EnrichmentItem> = batch.drain(..).collect();
        let batch_size = items.len();
        eprintln!("[enrichment-batcher] flushing batch of {batch_size} items");

        // 1. Batch embedding for items that need it
        let embed_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.needs_embedding)
            .map(|(i, _)| i)
            .collect();

        let embed_texts: Vec<String> = embed_indices
            .iter()
            .map(|&i| items[i].text.clone())
            .collect();

        let mut embed_results: Vec<Option<Vec<f32>>> = vec![None; items.len()];

        if !embed_texts.is_empty() {
            match self.llm.embed_voyage_batch(&embed_texts, "document").await {
                Ok(vecs) => {
                    for (vec_idx, &item_idx) in embed_indices.iter().enumerate() {
                        if vec_idx < vecs.len() {
                            embed_results[item_idx] = Some(vecs[vec_idx].clone());
                        }
                    }
                    eprintln!(
                        "[enrichment-batcher] batch embedded {} texts in 1 API call",
                        embed_texts.len()
                    );
                }
                Err(e) => {
                    eprintln!("[enrichment-batcher] batch embedding failed: {e}");
                }
            }
        }

        // 2. Generate summaries concurrently for items that need them
        let summary_futures: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.needs_summary)
            .map(|(i, item)| {
                let llm = self.llm.clone();
                let text = item.text.clone();
                async move { (i, llm.generate_summary(&text).await) }
            })
            .collect();

        let summary_results: Vec<(usize, Result<String, String>)> =
            futures::future::join_all(summary_futures).await;

        let mut summaries: Vec<Option<String>> = vec![None; items.len()];
        for (idx, result) in summary_results {
            match result {
                Ok(s) => summaries[idx] = Some(s),
                Err(e) => eprintln!(
                    "[enrichment-batcher] summary failed for {}: {e}",
                    items[idx].id
                ),
            }
        }

        // 3. Write results back to DB
        for (i, item) in items.iter().enumerate() {
            let new_vec = embed_results[i].as_deref();
            let new_summary = summaries[i].as_deref();

            if new_vec.is_some() || new_summary.is_some() {
                let update_action = |store: &mut MemoryStore| {
                    store
                        .update_enrichment_fields(&item.id, new_summary, new_vec, item.revision)
                        .map_err(|e| format!("Failed to update enriched entry: {e}"))
                };

                let res = if let Some(ref project_name) = item.named_project {
                    self.with_named_project_store(project_name, update_action)
                } else {
                    self.with_store_for_scope(item.target_db, update_action)
                };

                match res {
                    Ok(true) => {}
                    Ok(false) => eprintln!(
                        "[enrichment-batcher] discarded {} (revision changed)",
                        item.id
                    ),
                    Err(e) => {
                        eprintln!("[enrichment-batcher] DB update failed for {}: {e}", item.id)
                    }
                }
            }
        }

        eprintln!("[enrichment-batcher] batch of {batch_size} complete");
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

struct PubSubState {
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
        description = "Ghost-in-the-Shell style alias for save_memory. Write memory into cyberbrain."
    )]
    async fn cyberbrain_write(
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
        description = "Ghost-in-the-Shell style alias for search_memory. Query memories from cyberbrain."
    )]
    async fn cyberbrain_search(
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

    #[tool(description = "Set governance review status for a Hub capability.")]
    async fn hub_review(
        &self,
        Parameters(params): Parameters<HubReviewParams>,
    ) -> Result<String, String> {
        handle_hub_review(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for hub_review (Section 9 governance review)."
    )]
    async fn section9_review(
        &self,
        Parameters(params): Parameters<HubReviewParams>,
    ) -> Result<String, String> {
        handle_hub_review(self, params).await
    }

    #[tool(description = "Route an alias capability ID to a concrete active capability version.")]
    async fn hub_set_active_version(
        &self,
        Parameters(params): Parameters<HubSetActiveVersionParams>,
    ) -> Result<String, String> {
        handle_hub_set_active_version(self, params).await
    }

    #[tool(
        description = "Export Hub skills to agent-specific file formats. Targets: claude (SKILL.md + symlinks), openclaw (plugin manifest), cursor (.mdc rules), generic (raw files)."
    )]
    async fn hub_export_skills(
        &self,
        Parameters(params): Parameters<ExportSkillsParams>,
    ) -> Result<String, String> {
        handle_export_skills(self, params).await
    }

    #[tool(
        description = "Register a Virtual Capability (logical capability layer) on top of concrete backends."
    )]
    async fn vc_register(
        &self,
        Parameters(params): Parameters<VirtualCapabilityRegisterParams>,
    ) -> Result<String, String> {
        handle_vc_register(self, params).await
    }

    #[tool(
        description = "Bind a Virtual Capability to a concrete capability with deterministic priority and optional version pin."
    )]
    async fn vc_bind(
        &self,
        Parameters(params): Parameters<VirtualCapabilityBindParams>,
    ) -> Result<String, String> {
        handle_vc_bind(self, params).await
    }

    #[tool(description = "List Virtual Capabilities together with their current bindings.")]
    async fn vc_list(
        &self,
        Parameters(params): Parameters<HubDiscoverParams>,
    ) -> Result<String, String> {
        handle_vc_list(self, params).await
    }

    #[tool(
        description = "Resolve a Virtual Capability to the concrete capability currently selected for routing."
    )]
    async fn vc_resolve(
        &self,
        Parameters(params): Parameters<VirtualCapabilityResolveParams>,
    ) -> Result<String, String> {
        handle_vc_resolve(self, params).await
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

    #[tool(
        description = "Evolve a skill by analyzing its telemetry and using LLM to produce an improved prompt. Creates a new versioned capability."
    )]
    async fn skill_evolve(
        &self,
        Parameters(params): Parameters<SkillEvolveParams>,
    ) -> Result<String, String> {
        handle_skill_evolve(self, params).await
    }

    #[tool(
        description = "Recall structured memory context for an active agent turn. Returns ranked results plus a ready-to-inject prepend_context block."
    )]
    async fn recall_context(
        &self,
        Parameters(params): Parameters<RecallContextParams>,
    ) -> Result<String, String> {
        handle_recall_context(self, params).await
    }

    #[tool(
        description = "Capture durable memories from a recent session window. Extracts structured memories, embeds them inside Tachi, and writes them to the configured store."
    )]
    async fn capture_session(
        &self,
        Parameters(params): Parameters<CaptureSessionParams>,
    ) -> Result<String, String> {
        handle_capture_session(self, params).await
    }

    #[tool(
        description = "Synthesize agent evolution proposals from canonical profile documents and evidence. Returns structured JSON proposals; use dry_run=true to inspect the normalized request without calling the model."
    )]
    async fn synthesize_agent_evolution(
        &self,
        Parameters(params): Parameters<SynthesizeAgentEvolutionParams>,
    ) -> Result<String, String> {
        handle_synthesize_agent_evolution(self, params).await
    }

    #[tool(description = "View audit log of proxy tool calls through the Hub.")]
    async fn tachi_audit_log(
        &self,
        Parameters(params): Parameters<AuditLogParams>,
    ) -> Result<String, String> {
        handle_tachi_audit_log(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for tachi_audit_log (Section 9 audit view)."
    )]
    async fn section9_audit_log(
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

    #[tool(
        description = "Initialize a project-scoped Tachi memory DB under the current or target git repository."
    )]
    async fn tachi_init_project_db(
        &self,
        Parameters(params): Parameters<InitProjectDbParams>,
    ) -> Result<String, String> {
        handle_tachi_init_project_db(self, params).await
    }

    // ─── Agent Profile ───────────────────────────────────────────────────────

    #[tool(
        description = "Register this agent session with an identity profile. Enables per-agent memory scoping, tool filtering, and rate limit customization."
    )]
    async fn agent_register(
        &self,
        Parameters(params): Parameters<AgentRegisterParams>,
    ) -> Result<String, String> {
        let profile = AgentProfile {
            agent_id: params.agent_id.clone(),
            display_name: params
                .display_name
                .unwrap_or_else(|| params.agent_id.clone()),
            capabilities: params.capabilities,
            tool_filter: params.tool_filter,
            rate_limit_rpm: params.rate_limit_rpm,
            rate_limit_burst: params.rate_limit_burst,
            registered_at: Utc::now().to_rfc3339(),
        };

        let response = serde_json::to_string(&serde_json::json!({
            "status": "registered",
            "agent_id": profile.agent_id,
            "display_name": profile.display_name,
            "capabilities": profile.capabilities,
            "tool_filter": profile.tool_filter,
            "rate_limit_rpm": profile.rate_limit_rpm,
            "rate_limit_burst": profile.rate_limit_burst,
            "registered_at": profile.registered_at,
        }))
        .map_err(|e| format!("serialize: {e}"))?;

        let mut guard = self
            .agent_profile
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(profile);

        Ok(response)
    }

    #[tool(
        description = "Return the current agent profile for this session, or null if no agent has registered."
    )]
    async fn agent_whoami(
        &self,
        Parameters(_params): Parameters<AgentWhoamiParams>,
    ) -> Result<String, String> {
        let guard = self.agent_profile.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(profile) => serde_json::to_string(&profile).map_err(|e| format!("serialize: {e}")),
            None => Ok(r#"{"status":"unregistered","message":"No agent profile set. Call agent_register to identify this session."}"#.to_string()),
        }
    }

    // ─── Cross-Agent Handoff ─────────────────────────────────────────────────

    #[tool(
        description = "Leave a handoff memo for the next agent session. Contains session summary, next steps, and optional context."
    )]
    async fn handoff_leave(
        &self,
        Parameters(params): Parameters<HandoffLeaveParams>,
    ) -> Result<String, String> {
        let from_agent = {
            let guard = self.agent_profile.read().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(|p| p.agent_id.clone())
                .unwrap_or_else(|| "anonymous".to_string())
        };

        let memo = HandoffMemo {
            id: uuid::Uuid::new_v4().to_string(),
            from_agent: from_agent.clone(),
            target_agent: params.target_agent,
            summary: params.summary,
            next_steps: params.next_steps,
            context: params.context,
            created_at: Utc::now().to_rfc3339(),
            acknowledged: false,
        };

        let memo_id = memo.id.clone();
        let mut memos = self.handoff_memos.lock().unwrap_or_else(|e| e.into_inner());

        // Also persist to memory for cross-restart durability
        let memo_json = serde_json::to_string(&memo).map_err(|e| format!("serialize: {e}"))?;
        let metadata = crate::provenance::inject_provenance(
            self,
            serde_json::json!({"handoff_memo_id": memo_id.clone()}),
            "handoff_leave",
            "handoff_memo",
            Some("general"),
            DbScope::Global,
            serde_json::json!({
                "from_agent": from_agent.clone(),
                "target_agent": memo.target_agent.clone(),
                "next_steps_count": memo.next_steps.len(),
            }),
        );

        let entry = MemoryEntry {
            id: format!("handoff:{}", memo_id),
            text: format!(
                "[Handoff from {}] {}\n\nNext steps:\n{}",
                from_agent,
                memo.summary,
                memo.next_steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!("{}. {}", i + 1, s))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            category: "handoff".to_string(),
            importance: 0.9,
            summary: format!("Handoff from {}", from_agent),
            path: "/handoff".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            topic: "agent-handoff".to_string(),
            keywords: vec!["handoff".to_string(), from_agent.clone()],
            persons: vec![],
            entities: vec![from_agent.clone()],
            location: String::new(),
            source: "extraction".to_string(),
            scope: "general".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            vector: None,
            metadata,
        };
        self.with_global_store(|store| store.upsert(&entry).map_err(|e| format!("{e}")))?;

        memos.push(memo);

        // Keep only last 50 memos
        if memos.len() > 50 {
            let drain_count = memos.len() - 50;
            memos.drain(..drain_count);
        }

        serde_json::to_string(&json!({
            "status": "memo_left",
            "memo_id": memo_id,
            "from_agent": from_agent,
            "memo": memo_json,
        }))
        .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(
        description = "Check for pending handoff memos from previous agent sessions. Call this at the start of a new session."
    )]
    async fn handoff_check(
        &self,
        Parameters(params): Parameters<HandoffCheckParams>,
    ) -> Result<String, String> {
        let mut memos = self.handoff_memos.lock().unwrap_or_else(|e| e.into_inner());

        let matching: Vec<&HandoffMemo> = memos
            .iter()
            .filter(|m| {
                if m.acknowledged {
                    return false;
                }
                match (&params.agent_id, &m.target_agent) {
                    (_, None) => true, // Memo for any agent
                    (Some(my_id), Some(target)) => my_id == target,
                    (None, _) => true, // No filter, return all
                }
            })
            .collect();

        let result = serde_json::to_string(&json!({
            "pending_memos": matching.len(),
            "memos": matching,
        }))
        .map_err(|e| format!("serialize: {e}"))?;

        // Mark as acknowledged if requested
        if params.acknowledge {
            let agent_filter = params.agent_id.as_deref();
            for memo in memos.iter_mut() {
                if memo.acknowledged {
                    continue;
                }
                let matches = match (&agent_filter, &memo.target_agent) {
                    (_, None) => true,
                    (Some(my_id), Some(target)) => *my_id == *target,
                    (None, _) => true,
                };
                if matches {
                    memo.acknowledged = true;
                }
            }
        }

        Ok(result)
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
        description = "Ghost-in-the-Shell style alias for ghost_publish. Send a ghost whisper to a topic."
    )]
    async fn ghost_whisper(
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
        description = "Ghost-in-the-Shell style alias for ghost_subscribe. Listen for new whispers."
    )]
    async fn ghost_listen(
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

    #[tool(
        description = "Ghost-in-the-Shell style alias for ghost_topics. List active whisper channels."
    )]
    async fn ghost_channels(&self) -> Result<String, String> {
        handle_ghost_topics(self).await
    }

    #[tool(
        description = "Acknowledge a Ghost topic cursor for an agent. Supports explicit index or message_id."
    )]
    async fn ghost_ack(
        &self,
        Parameters(params): Parameters<GhostAckParams>,
    ) -> Result<String, String> {
        handle_ghost_ack(self, params).await
    }

    #[tool(
        description = "Write a Ghost reflection entry and optionally promote it into derived rules."
    )]
    async fn ghost_reflect(
        &self,
        Parameters(params): Parameters<GhostReflectParams>,
    ) -> Result<String, String> {
        handle_ghost_reflect(self, params).await
    }

    #[tool(
        description = "Promote a Ghost message into long-term memory and mark the message as promoted."
    )]
    async fn ghost_promote(
        &self,
        Parameters(params): Parameters<GhostPromoteParams>,
    ) -> Result<String, String> {
        handle_ghost_promote(self, params).await
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

    #[tool(
        description = "Set runtime sandbox policy for a capability (timeouts, concurrency, env allowlist, fs/cwd roots)."
    )]
    async fn sandbox_set_policy(
        &self,
        Parameters(params): Parameters<SandboxSetPolicyParams>,
    ) -> Result<String, String> {
        handle_sandbox_set_policy(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for sandbox_set_policy. Configure shell execution policy."
    )]
    async fn shell_set_policy(
        &self,
        Parameters(params): Parameters<SandboxSetPolicyParams>,
    ) -> Result<String, String> {
        handle_sandbox_set_policy(self, params).await
    }

    #[tool(description = "Get runtime sandbox policy for a capability.")]
    async fn sandbox_get_policy(
        &self,
        Parameters(params): Parameters<SandboxGetPolicyParams>,
    ) -> Result<String, String> {
        handle_sandbox_get_policy(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for sandbox_get_policy. Read shell execution policy."
    )]
    async fn shell_get_policy(
        &self,
        Parameters(params): Parameters<SandboxGetPolicyParams>,
    ) -> Result<String, String> {
        handle_sandbox_get_policy(self, params).await
    }

    #[tool(description = "List runtime sandbox policies.")]
    async fn sandbox_list_policies(
        &self,
        Parameters(params): Parameters<SandboxListPoliciesParams>,
    ) -> Result<String, String> {
        handle_sandbox_list_policies(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for sandbox_list_policies. List shell policies."
    )]
    async fn shell_list_policies(
        &self,
        Parameters(params): Parameters<SandboxListPoliciesParams>,
    ) -> Result<String, String> {
        handle_sandbox_list_policies(self, params).await
    }

    #[tool(
        description = "List sandbox execution audit rows (policy decisions, startup, runtime outcomes)."
    )]
    async fn sandbox_exec_audit(
        &self,
        Parameters(params): Parameters<SandboxExecAuditParams>,
    ) -> Result<String, String> {
        handle_sandbox_exec_audit(self, params).await
    }

    #[tool(
        description = "Ghost-in-the-Shell style alias for sandbox_exec_audit. Inspect shell execution audit."
    )]
    async fn shell_exec_audit(
        &self,
        Parameters(params): Parameters<SandboxExecAuditParams>,
    ) -> Result<String, String> {
        handle_sandbox_exec_audit(self, params).await
    }

    // ─── Pack System Tools ────────────────────────────────────────────────────

    #[tool(description = "List installed skill packs. Optionally filter by enabled_only.")]
    async fn pack_list(
        &self,
        Parameters(params): Parameters<PackListParams>,
    ) -> Result<String, String> {
        handle_pack_list(self, params).await
    }

    #[tool(description = "Get details of a single installed skill pack by ID.")]
    async fn pack_get(
        &self,
        Parameters(params): Parameters<PackGetParams>,
    ) -> Result<String, String> {
        handle_pack_get(self, params).await
    }

    #[tool(
        description = "Register a skill pack after git clone / download. Records the pack in the registry with its metadata, source, and skill count."
    )]
    async fn pack_register(
        &self,
        Parameters(params): Parameters<PackRegisterParams>,
    ) -> Result<String, String> {
        handle_pack_register(self, params).await
    }

    #[tool(
        description = "Remove a skill pack from the registry. Also cleans up projected files in agent directories unless clean_files=false."
    )]
    async fn pack_remove(
        &self,
        Parameters(params): Parameters<PackRemoveParams>,
    ) -> Result<String, String> {
        handle_pack_remove(self, params).await
    }

    #[tool(
        description = "Project a pack's skills, workflows, and host overlays to one or more agents. Converts SKILL.md files to each agent's native format (e.g. .mdc rules for Cursor) and emits a tachi-projection manifest for adapters such as OpenClaw."
    )]
    async fn pack_project(
        &self,
        Parameters(params): Parameters<PackProjectParams>,
    ) -> Result<String, String> {
        handle_pack_project(self, params).await
    }

    #[tool(description = "List agent projections. Filter by agent and/or pack_id.")]
    async fn projection_list(
        &self,
        Parameters(params): Parameters<ProjectionListParams>,
    ) -> Result<String, String> {
        handle_projection_list(self, params).await
    }

    // ─── Vault (Encrypted Secret Storage) ────────────────────────────────────

    #[tool(description = "Initialize the vault with a master password. Can only be called once.")]
    async fn vault_init(
        &self,
        Parameters(params): Parameters<VaultInitParams>,
    ) -> Result<String, String> {
        handle_vault_init(self, params).await
    }

    #[tool(description = "Unlock the vault by verifying the master password.")]
    async fn vault_unlock(
        &self,
        Parameters(params): Parameters<VaultUnlockParams>,
    ) -> Result<String, String> {
        handle_vault_unlock(self, params).await
    }

    #[tool(description = "Lock the vault (clear encryption key from memory).")]
    async fn vault_lock(&self) -> Result<String, String> {
        handle_vault_lock(self).await
    }

    #[tool(
        description = "Store or update an encrypted secret in the vault. Supports multi-key rotation when name ends with _N."
    )]
    async fn vault_set(
        &self,
        Parameters(params): Parameters<VaultSetParams>,
    ) -> Result<String, String> {
        handle_vault_set(self, params).await
    }

    #[tool(
        description = "Retrieve and decrypt a secret from the vault. Supports auto-rotation for multi-key secrets."
    )]
    async fn vault_get(
        &self,
        Parameters(params): Parameters<VaultGetParams>,
    ) -> Result<String, String> {
        handle_vault_get(self, params).await
    }

    #[tool(
        description = "List all stored secrets (names and metadata only, not values). Does not require vault to be unlocked."
    )]
    async fn vault_list(
        &self,
        Parameters(params): Parameters<VaultListParams>,
    ) -> Result<String, String> {
        handle_vault_list(self, params).await
    }

    #[tool(description = "Delete a secret from the vault.")]
    async fn vault_remove(
        &self,
        Parameters(params): Parameters<VaultRemoveParams>,
    ) -> Result<String, String> {
        handle_vault_remove(self, params).await
    }

    #[tool(description = "Check vault status (initialized, locked/unlocked, entry count).")]
    async fn vault_status(&self) -> Result<String, String> {
        handle_vault_status(self).await
    }

    #[tool(
        description = "Setup key rotation for a prefix. Requires keys like PREFIX_1, PREFIX_2, etc. to already exist."
    )]
    async fn vault_setup_rotation(
        &self,
        Parameters(params): Parameters<VaultSetupRotationParams>,
    ) -> Result<String, String> {
        handle_vault_setup_rotation(self, params).await
    }
}

impl MemoryServer {
    /// Atomically check if connection exists and create if not.
    /// Prevents TOCTOU race where two concurrent calls both spawn a child.
    #[allow(dead_code)]
    async fn ensure_child_connected(&self, server_name: &str) -> Result<(), rmcp::ErrorData> {
        self.ensure_child_connected_with_context(&format!("mcp:{server_name}"), None)
            .await
    }

    async fn ensure_child_connected_with_context(
        &self,
        resolved_capability_id: &str,
        requested_capability_id: Option<&str>,
    ) -> Result<(), rmcp::ErrorData> {
        let server_name = resolved_capability_id
            .strip_prefix("mcp:")
            .unwrap_or(resolved_capability_id);
        // Check under lock
        {
            let conns = lock_or_recover(&self.pool.connections, "mcp_pool.connections");
            if conns.contains_key(server_name) {
                return Ok(());
            }
        }
        // Not connected — acquire connecting lock to serialize connection attempts
        let connecting_lock = {
            let mut locks =
                lock_or_recover(&self.pool.connecting_locks, "mcp_pool.connecting_locks");
            locks
                .entry(server_name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = connecting_lock.lock().await;
        // Double-check after acquiring lock
        {
            let conns = lock_or_recover(&self.pool.connections, "mcp_pool.connections");
            if conns.contains_key(server_name) {
                return Ok(());
            }
        }
        self.connect_child_with_context(resolved_capability_id, requested_capability_id)
            .await
    }

    #[allow(dead_code)]
    async fn connect_child(&self, server_name: &str) -> Result<(), rmcp::ErrorData> {
        self.connect_child_with_context(&format!("mcp:{server_name}"), None)
            .await
    }

    async fn connect_child_with_context(
        &self,
        resolved_capability_id: &str,
        requested_capability_id: Option<&str>,
    ) -> Result<(), rmcp::ErrorData> {
        let server_id = resolved_capability_id.to_string();
        let server_name = resolved_capability_id
            .strip_prefix("mcp:")
            .unwrap_or(resolved_capability_id);

        let cap = self.get_capability(&server_id)?;
        let (sandbox_policy, policy_source) =
            self.get_effective_sandbox_policy(requested_capability_id, &server_id);
        if sandbox_policy.is_none() {
            self.record_sandbox_exec_audit(
                &server_id,
                "preflight",
                "denied",
                Some("missing sandbox policy"),
                0,
                None,
                Some("policy_missing"),
                &json!({
                    "server_name": server_name,
                    "requested_capability_id": requested_capability_id,
                }),
            );
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "Capability '{}' has no sandbox policy. Use sandbox_set_policy before connecting.",
                    server_id
                ),
                None,
            ));
        }
        if sandbox_policy
            .as_ref()
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            self.record_sandbox_exec_audit(
                &server_id,
                "preflight",
                "denied",
                Some("sandbox policy disabled capability"),
                0,
                None,
                Some("policy_disabled"),
                &json!({
                    "server_name": server_name,
                    "requested_capability_id": requested_capability_id,
                    "policy_source": if policy_source.is_empty() { None::<String> } else { Some(policy_source.clone()) },
                }),
            );
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "Capability '{}' blocked by sandbox policy (enabled=false)",
                    server_id
                ),
                None,
            ));
        }
        if !capability_callable(&cap) {
            self.record_sandbox_exec_audit(
                &server_id,
                "preflight",
                "denied",
                Some("capability is not callable"),
                0,
                None,
                Some("capability_not_callable"),
                &json!({
                    "server_name": server_name,
                    "requested_capability_id": requested_capability_id,
                    "enabled": cap.enabled,
                    "review_status": cap.review_status,
                    "health_status": cap.health_status,
                }),
            );
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "MCP server '{}' is not callable (enabled={}, review_status={}, health_status={}).",
                    server_id, cap.enabled, cap.review_status, cap.health_status
                ),
                None,
            ));
        }

        let def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("bad definition: {e}"), None))?;
        let startup_timeout_ms = def["startup_timeout_ms"].as_u64().unwrap_or(30_000);
        let startup_timeout = Duration::from_millis(startup_timeout_ms.max(1));
        let client = self
            .connect_mcp_service(&server_id, requested_capability_id, &def, startup_timeout)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;

        lock_or_recover(&self.pool.connections, "mcp_pool.connections").insert(
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
        self.proxy_call_capability_internal(
            &format!("mcp:{server_name}"),
            None,
            tool_name,
            arguments,
        )
        .await
    }

    async fn proxy_call_capability_internal(
        &self,
        resolved_capability_id: &str,
        requested_capability_id: Option<&str>,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        // 0. Look up capability for deny-list and timeout config
        let server_id = resolved_capability_id.to_string();
        let server_name = resolved_capability_id
            .strip_prefix("mcp:")
            .unwrap_or(resolved_capability_id);
        let args_hash = stable_hash(&format!("{:?}", arguments));
        let audit_reject = |error_kind: &str| {
            let timestamp = Utc::now().to_rfc3339();
            let _ = self.with_global_store(|store| {
                store
                    .audit_log_insert(
                        &timestamp,
                        server_name,
                        tool_name,
                        &args_hash,
                        false,
                        0,
                        Some(error_kind),
                    )
                    .map_err(|e| format!("{e}"))
            });
        };
        let cap = self.get_capability(&server_id)?;
        if !capability_callable(&cap) {
            audit_reject("capability_not_callable");
            self.record_sandbox_exec_audit(
                &server_id,
                "preflight",
                "denied",
                Some("capability is not callable"),
                0,
                Some(tool_name),
                Some("capability_not_callable"),
                &json!({
                    "server_name": server_name,
                    "enabled": cap.enabled,
                    "review_status": cap.review_status,
                    "health_status": cap.health_status,
                }),
            );
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "MCP server '{}' is not callable (enabled={}, review_status={}, health_status={}).",
                    server_id, cap.enabled, cap.review_status, cap.health_status
                ),
                None,
            ));
        }
        let cap_def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("bad definition: {e}"), None))?;
        let (sandbox_policy, policy_source) =
            self.get_effective_sandbox_policy(requested_capability_id, &server_id);
        if sandbox_policy.is_none() {
            audit_reject("policy_missing");
            self.record_sandbox_exec_audit(
                &server_id,
                "preflight",
                "denied",
                Some("missing sandbox policy"),
                0,
                Some(tool_name),
                Some("policy_missing"),
                &json!({
                    "server_name": server_name,
                    "requested_capability_id": requested_capability_id,
                }),
            );
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "Capability '{}' has no sandbox policy. Use sandbox_set_policy before calling.",
                    server_id
                ),
                None,
            ));
        }
        if let Some(policy) = sandbox_policy.as_ref() {
            let policy_enabled = policy
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !policy_enabled {
                audit_reject("policy_disabled");
                self.record_sandbox_exec_audit(
                    &server_id,
                    "preflight",
                    "denied",
                    Some("sandbox policy disabled capability"),
                    0,
                    Some(tool_name),
                    Some("policy_disabled"),
                    &json!({
                        "server_name": server_name,
                        "requested_capability_id": requested_capability_id,
                        "policy_source": if policy_source.is_empty() { None::<String> } else { Some(policy_source.clone()) },
                    }),
                );
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Capability '{}' blocked by sandbox policy (enabled=false)",
                        server_id
                    ),
                    None,
                ));
            }
        }

        // 1. Check allow/deny permissions
        if let Some(allow_list) = cap_def["permissions"]["allow"].as_array() {
            let allowed: HashSet<&str> = allow_list.iter().filter_map(|v| v.as_str()).collect();
            if !allowed.is_empty() && !allowed.contains(tool_name) {
                audit_reject("permission_allow_denied");
                self.record_sandbox_exec_audit(
                    &server_id,
                    "preflight",
                    "denied",
                    Some("tool not in permissions.allow"),
                    0,
                    Some(tool_name),
                    Some("permission_allow_denied"),
                    &json!({
                        "server_name": server_name,
                        "requested_capability_id": requested_capability_id,
                    }),
                );
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
                audit_reject("permission_deny_blocked");
                self.record_sandbox_exec_audit(
                    &server_id,
                    "preflight",
                    "denied",
                    Some("tool denied by permissions policy"),
                    0,
                    Some(tool_name),
                    Some("permission_deny_blocked"),
                    &json!({
                        "server_name": server_name,
                        "requested_capability_id": requested_capability_id,
                    }),
                );
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
            let mut circuits = lock_or_recover(&self.pool.circuits, "mcp_pool.circuits");
            if let Some((state, count)) = circuits.get_mut(server_name) {
                match state {
                    CircuitState::Open { until } => {
                        if Instant::now() < *until {
                            audit_reject("circuit_open");
                            self.record_sandbox_exec_audit(
                                &server_id,
                                "preflight",
                                "denied",
                                Some("circuit breaker open"),
                                0,
                                Some(tool_name),
                                Some("circuit_open"),
                                &json!({
                                    "server_name": server_name,
                                    "requested_capability_id": requested_capability_id,
                                }),
                            );
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
            let mut sems = lock_or_recover(&self.pool.semaphores, "mcp_pool.semaphores");
            let mut max_conc = cap_def["max_concurrency"].as_u64().unwrap_or(1);
            if let Some(policy_cap) = sandbox_policy
                .as_ref()
                .and_then(|v| v.get("max_concurrency"))
                .and_then(|v| v.as_u64())
            {
                max_conc = std::cmp::min(max_conc.max(1), policy_cap.max(1));
            }
            let max_conc = max_conc.max(1) as usize;
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
        self.ensure_child_connected_with_context(&server_id, requested_capability_id)
            .await?;

        // 5. Get peer and call tool with timeout
        let mut call_params = rmcp::model::CallToolRequestParams::new(tool_name.to_string());
        if let Some(ref args) = arguments {
            call_params = call_params.with_arguments(args.clone());
        }

        let peer = {
            let mut conns = lock_or_recover(&self.pool.connections, "mcp_pool.connections");
            if let Some(conn) = conns.get_mut(server_name) {
                conn.last_used = Instant::now();
                conn.client.peer().clone()
            } else {
                return Err(rmcp::ErrorData::internal_error("connection lost", None));
            }
        };

        let mut timeout_ms = cap_def["tool_timeout_ms"].as_u64().unwrap_or(30000);
        if let Some(policy_tool_ms) = sandbox_policy
            .as_ref()
            .and_then(|v| v.get("max_tool_ms"))
            .and_then(|v| v.as_u64())
        {
            timeout_ms = std::cmp::min(timeout_ms.max(1), policy_tool_ms.max(1));
        }
        let start = Instant::now();

        let result = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            peer.call_tool(call_params),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        // 6. Process result, update circuit breaker, log audit
        let (final_result, sandbox_decision, sandbox_error_kind) = match result {
            Ok(Ok(r)) => {
                // Tool returned successfully (even if r.is_error — that's a tool-level error, not transport)
                let mut circuits = lock_or_recover(&self.pool.circuits, "mcp_pool.circuits");
                circuits.insert(server_name.to_string(), (CircuitState::Closed, 0));
                (Ok(r), "allowed", None)
            }
            Ok(Err(e)) => {
                // Transport/protocol error — increment circuit breaker
                self.record_circuit_failure(server_name);
                (
                    Err(rmcp::ErrorData::internal_error(
                        format!("proxy call failed: {e}"),
                        None,
                    )),
                    "failed",
                    Some("proxy_failed"),
                )
            }
            Err(_timeout) => {
                // Timeout — increment circuit breaker
                self.record_circuit_failure(server_name);
                (
                    Err(rmcp::ErrorData::internal_error(
                        format!(
                            "Tool call '{}' on '{}' timed out after {}ms",
                            tool_name, server_name, timeout_ms
                        ),
                        None,
                    )),
                    "timeout",
                    Some("tool_timeout"),
                )
            }
        };

        // 7. Audit log (fire and forget)
        let success = final_result.is_ok();
        let error_kind = final_result.as_ref().err().map(|e| format!("{e}"));
        let timestamp = Utc::now().to_rfc3339();
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
        self.record_sandbox_exec_audit(
            &server_id,
            "tool_call",
            sandbox_decision,
            error_kind.as_deref(),
            duration_ms,
            Some(tool_name),
            sandbox_error_kind,
            &json!({
                "server_name": server_name,
                "requested_capability_id": requested_capability_id,
                "policy_source": if policy_source.is_empty() { None::<String> } else { Some(policy_source) },
                "timeout_ms": timeout_ms,
            }),
        );

        final_result
    }

    fn record_circuit_failure(&self, server_name: &str) {
        let mut should_remove = false;
        {
            let mut circuits = lock_or_recover(&self.pool.circuits, "mcp_pool.circuits");
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
            lock_or_recover(&self.pool.connections, "mcp_pool.connections").remove(server_name);
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
