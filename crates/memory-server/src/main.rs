// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod llm;
mod prompts;

use chrono::Utc;
use clap::Parser;
use memory_core::{HubCapability, MemoryEntry, MemoryStore, SearchOptions};
use rmcp::{
    model::{ServerCapabilities, ServerInfo},
    tool, tool_router,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    transport::StreamableHttpClientTransport,
    ServerHandler,
    schemars,
    schemars::JsonSchema,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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

/// Tools whose results can be cached (read-only, no side effects)
const CACHEABLE_TOOLS: &[&str] = &[
    "search_memory",
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
        found.ok_or_else(|| rmcp::ErrorData::invalid_params(format!("Capability '{cap_id}' not found"), None))
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
                rmcp::ErrorData::invalid_params(format!("Skill tool '{}' not found", tool_name), None)
            })?;

        let cap = self.get_capability(&skill_id)?;
        let def: Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("Invalid skill definition JSON: {e}"), None))?;

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

        let output = if let Some(mock_response) = def.get("mock_response").and_then(|v| v.as_str()) {
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
                .map_err(|e| rmcp::ErrorData::internal_error(format!("skill execution failed: {e}"), None))?
        };

        make_text_tool_result(&json!({
            "skill_id": skill_id,
            "tool_name": tool_name,
            "output": output
        }))
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
        "npx", "node", "bun", "deno",
        "python3", "python", "uv",
        "cargo", "rustup",
        "docker", "podman",
        "tachi",
    ];

    if TRUSTED_BASENAMES.contains(&basename) {
        return true;
    }

    // Allow absolute paths under Homebrew, nvm, cargo, common bin dirs
    const TRUSTED_PREFIXES: &[&str] = &[
        "/opt/homebrew/",
        "/usr/local/bin/",
        "/usr/bin/",
        "/bin/",
    ];

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

fn resolve_env_map(def: &serde_json::Value) -> Result<HashMap<String, String>, String> {
    let mut result = HashMap::new();
    if let Some(obj) = def["env"].as_object() {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                let resolved = if val.starts_with("${") && val.ends_with('}') {
                    let var_name = &val[2..val.len() - 1];
                    std::env::var(var_name).map_err(|_| {
                        format!("Environment variable '{}' not set (required by MCP server)", var_name)
                    })?
                } else {
                    val.to_string()
                };
                result.insert(k.clone(), resolved);
            }
        }
    }
    Ok(result)
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

    /// Optional entry ID (for updates)
    #[serde(default)]
    id: Option<String>,
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
struct SearchMemoryParams {
    /// Search query text
    query: String,

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

    /// Graph expand hops (0 = disabled)
    #[serde(default)]
    graph_expand_hops: u32,

    /// Optional relation filter for graph expansion
    #[serde(default)]
    graph_relation_filter: Option<String>,
}

impl SearchMemoryParams {
    /// Build SearchOptions from params, only differing by vec_available per DB.
    fn to_search_options(&self, vec_available: bool) -> SearchOptions {
        SearchOptions {
            top_k: self.top_k,
            path_prefix: self.path_prefix.clone(),
            include_archived: self.include_archived,
            candidates_per_channel: self.candidates_per_channel,
            mmr_threshold: self.mmr_threshold,
            graph_expand_hops: self.graph_expand_hops,
            graph_relation_filter: self.graph_relation_filter.clone(),
            vec_available,
            ..Default::default()
        }
    }
}

/// Build a MemoryEntry from a JSON fact value (shared by extract_facts and ingest_event).
fn fact_to_entry(fact: &serde_json::Value, source: &str, metadata: serde_json::Value) -> Option<MemoryEntry> {
    let text = fact["text"].as_str().unwrap_or("").to_string();
    if text.is_empty() {
        return None;
    }
    let topic = fact["topic"].as_str().unwrap_or("").to_string();
    let importance = fact["importance"].as_f64().unwrap_or(0.7);
    let keywords: Vec<String> = fact["keywords"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let scope = fact["scope"].as_str().unwrap_or("general").to_string();
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

// ─── Tool Implementations ────────────────────────────────────────────────────────

#[tool_router]
impl MemoryServer {
    #[tool(description = "Save a memory entry to the store. Creates a new entry or updates an existing one if id is provided.")]
    async fn save_memory(
        &self,
        Parameters(params): Parameters<SaveMemoryParams>,
    ) -> Result<String, String> {
        let id = params.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let timestamp = Utc::now().to_rfc3339();
        let requested_scope = params.scope.clone();
        let (target_db, warning) = self.resolve_write_scope(&requested_scope);

        // Use caller-provided summary or leave empty for background enrichment
        let summary = params.summary;
        let needs_summary = summary.is_empty();

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
            vector: None,
        };

        // Write immediately with empty summary/vector
        self.with_store_for_scope(target_db, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory: {}", e))
        })?;

        // Spawn background enrichment (embedding + summary)
        let server = self.clone();
        let enrich_id = id.clone();
        let enrich_text = entry.text.clone();
        tokio::spawn(async move {
            let mut enriched_entry = entry;

            // Generate embedding
            match server.llm.embed_voyage(&enrich_text, "document").await {
                Ok(vec) => enriched_entry.vector = Some(vec),
                Err(e) => eprintln!("[enrichment] embedding failed for {enrich_id}: {e}"),
            }

            // Generate summary if not provided
            if needs_summary {
                match server.llm.generate_summary(&enrich_text).await {
                    Ok(s) => enriched_entry.summary = s,
                    Err(e) => eprintln!("[enrichment] summary failed for {enrich_id}: {e}"),
                }
            }

            // Update entry with enriched data
            if let Err(e) = server.with_store_for_scope(target_db, |store| {
                store
                    .upsert(&enriched_entry)
                    .map_err(|e| format!("Failed to update enriched entry: {e}"))
            }) {
                eprintln!("[enrichment] DB update failed for {enrich_id}: {e}");
            } else {
                eprintln!("[enrichment] completed for {enrich_id}");
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

        serde_json::to_string(&serde_json::Value::Object(response))
            .map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Search memory entries using hybrid search (vector + FTS + symbolic). Returns ranked results with scores.")]
    async fn search_memory(
        &self,
        Parameters(params): Parameters<SearchMemoryParams>,
    ) -> Result<String, String> {
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
        databases.insert("global".into(), json!({
            "path": self.global_db_path.display().to_string(),
            "vec_available": self.global_vec_available,
            "total": global_stats.total,
            "by_scope": global_stats.by_scope,
            "by_category": global_stats.by_category,
        }));
        if let Some(ref ps) = project_stats {
            databases.insert("project".into(), json!({
                "path": self.project_db_path.as_ref().map(|p| p.display().to_string()),
                "vec_available": self.project_vec_available,
                "total": ps.total,
                "by_scope": ps.by_scope,
                "by_category": ps.by_category,
            }));
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
    async fn extract_facts(&self, Parameters(params): Parameters<ExtractFactsParams>) -> Result<String, String> {
        let (target_db, _warning) = self.resolve_write_scope("project");

        // Spawn LLM extraction in background
        let server = self.clone();
        let text = params.text.clone();
        let source = params.source.clone();
        tokio::spawn(async move {
            match server.llm.extract_facts(&text).await {
                Ok(facts) if facts.is_empty() => {
                    eprintln!("[extract_facts] no facts extracted");
                }
                Ok(facts) => {
                    let count = facts.len();
                    let saved = server.with_store_for_scope(target_db, |store| {
                        let mut saved = 0;
                        for fact in &facts {
                            let Some(entry) = fact_to_entry(
                                fact,
                                "extraction",
                                serde_json::json!({"source": source}),
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
                        Ok(n) => eprintln!("[extract_facts] saved {n}/{count} facts"),
                        Err(e) => eprintln!("[extract_facts] DB write failed: {e}"),
                    }
                }
                Err(e) => eprintln!("[extract_facts] LLM extraction failed: {e}"),
            }
        });

        Ok("extraction queued".to_string())
    }

    #[tool(description = "Ingest a conversation event and extract facts from messages.")]
    async fn ingest_event(&self, Parameters(params): Parameters<IngestEventParams>) -> Result<String, String> {
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
            return Ok("No content to process.".to_string());
        }

        // Atomic dedup: try to claim the event first via INSERT OR IGNORE.
        // If another concurrent call already claimed it, we skip.
        let claimed = self.with_store_for_scope(target_db, |store| {
            store
                .try_claim_event(&event_hash, &format!("{}:{}", params.conversation_id, params.turn_id), "ingest")
                .map_err(|e| format!("Failed to claim event: {e}"))
        })?;
        if !claimed {
            return Ok(format!("Event already processed (hash: {})", event_hash));
        }

        // Spawn LLM fact extraction in background
        let server = self.clone();
        let conversation_id = params.conversation_id.clone();
        let turn_id = params.turn_id.clone();
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
                        Err(e) => eprintln!("[ingest_event] DB write failed: {e}"),
                    }
                }
                Err(e) => eprintln!("[ingest_event] LLM extraction failed for {conversation_id}:{turn_id}: {e}"),
            }
        });

        Ok(format!("event claimed, processing in background (hash: {})", event_hash))
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

        let cache_size = self.tool_cache.lock().unwrap_or_else(|e| e.into_inner()).len();
        let hits = self.cache_hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self.cache_misses.load(std::sync::atomic::Ordering::Relaxed);

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
        }))
        .map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Get only new or changed memories since last sync for this agent. Returns incremental diff to save tokens. Use agent_id to identify your agent uniquely.")]
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
            let _ = self.with_global_store(|store| {
                store
                    .update_agent_known_state(&params.agent_id, &sync_updates)
                    .map_err(|e| format!("Failed to update agent state: {}", e))
            });
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

    #[tool(description = "Register a capability (skill, plugin, or MCP server) in the Hub.")]
    async fn hub_register(&self, Parameters(params): Parameters<HubRegisterParams>) -> Result<String, String> {
        let (target_db, warning) = self.resolve_write_scope(&params.scope);

        // Security: validate MCP server commands against allowlist
        let auto_enabled = if params.cap_type == "mcp" {
            let def: serde_json::Value = serde_json::from_str(&params.definition).unwrap_or_default();
            if def["transport"].as_str().unwrap_or("stdio") == "stdio" {
                if let Some(cmd) = def["command"].as_str() {
                    is_trusted_command(cmd)
                } else {
                    false
                }
            } else {
                true // SSE/HTTP are URLs, no local exec risk
            }
        } else {
            true
        };
        let cap = HubCapability {
            id: params.id.clone(),
            cap_type: params.cap_type.clone(),
            name: params.name.clone(),
            version: params.version,
            description: params.description.clone(),
            definition: params.definition.clone(),
            enabled: auto_enabled,
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

        let mut resp = serde_json::Map::new();
        resp.insert("id".into(), json!(params.id));
        resp.insert("db".into(), json!(target_db.as_str()));
        resp.insert("version".into(), json!(params.version));
        if let Some(w) = warning {
            resp.insert("warning".into(), json!(w));
        }
        if !auto_enabled {
            let def: serde_json::Value = serde_json::from_str(&params.definition).unwrap_or_default();
            let cmd = def["command"].as_str().unwrap_or("unknown");
            resp.insert("warning".into(), json!(format!(
                "Command '{}' is not in the trusted allowlist. Capability registered but disabled. Use hub_set_enabled to activate after review.", cmd
            )));
            resp.insert("enabled".into(), json!(false));
        }

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
            let def: serde_json::Value = serde_json::from_str(&params.definition).unwrap_or_default();
            if def["prompt"].as_str().is_some() {
                let llm = self.llm.clone();
                let cap_clone = cap.clone();
                let desc_empty = params.description.is_empty();
                let db_path = match target_db {
                    DbScope::Global => self.global_db_path.clone(),
                    DbScope::Project => self.project_db_path.clone().unwrap_or_else(|| self.global_db_path.clone()),
                };
                let prompt_text = def["prompt"].as_str().unwrap().to_string();

                let cap_id = cap_clone.id.clone();

                tokio::spawn(async move {
                    match llm.call_llm(
                        crate::prompts::SKILL_ANALYSIS_PROMPT,
                        &prompt_text,
                        None,
                        0.3,
                        500,
                    ).await {
                        Ok(analysis_raw) => {
                            let analysis_json: serde_json::Value = serde_json::from_str(
                                llm::LlmClient::strip_code_fence(&analysis_raw)
                            ).unwrap_or(serde_json::json!({"summary": analysis_raw}));

                            // Auto-fill description if it was empty
                            if desc_empty {
                                if let Some(summary) = analysis_json["summary"].as_str() {
                                    let mut updated_cap = cap_clone;
                                    updated_cap.description = summary.to_string();
                                    if let Ok(store) = MemoryStore::open(db_path.to_str().unwrap_or("")) {
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
        } else if params.cap_type == "mcp" {
            // Auto-discover tools from child MCP server
            let def: serde_json::Value = serde_json::from_str(&cap.definition).unwrap_or_default();
            let transport_type = def["transport"].as_str().unwrap_or("stdio");
            let server_name = params.id.strip_prefix("mcp:").unwrap_or(&params.id);

            match transport_type {
                "stdio" => {
                    if let (Some(command), args) = (
                        def["command"].as_str(),
                        def["args"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                    ) {
                        let env_map = match resolve_env_map(&def) {
                            Ok(m) => m,
                            Err(e) => {
                                resp.insert("discovery_error".into(), json!(e));
                                HashMap::new()
                            }
                        };

                        let mut cmd = tokio::process::Command::new(command);
                        cmd.args(&args).env_clear();
                        if let Ok(path) = std::env::var("PATH") {
                            cmd.env("PATH", &path);
                        }
                        if let Ok(home) = std::env::var("HOME") {
                            cmd.env("HOME", &home);
                        }
                        for (k, v) in &env_map {
                            cmd.env(k, v);
                        }

                        match rmcp::transport::TokioChildProcess::new(cmd) {
                            Ok(transport) => match rmcp::ServiceExt::serve((), transport).await {
                                Ok(client) => match client.peer().list_all_tools().await {
                                    Ok(tools) => {
                                        let tools_count = tools.len();

                                        self.proxy_tools
                                            .lock()
                                            .unwrap()
                                            .insert(server_name.to_string(), tools.clone());

                                        let mut updated_def = def.clone();
                                        updated_def["discovered_tools"] =
                                            serde_json::to_value(&tools).unwrap_or_default();
                                        updated_def["discovered_at"] = json!(Utc::now().to_rfc3339());
                                        updated_def["tools_count"] = json!(tools_count);

                                        let updated_cap = HubCapability {
                                            definition: serde_json::to_string(&updated_def)
                                                .unwrap_or(cap.definition.clone()),
                                            ..cap.clone()
                                        };
                                        let _ = self.with_store_for_scope(target_db, |store| {
                                            store
                                                .hub_register(&updated_cap)
                                                .map_err(|e| format!("update: {e}"))
                                        });

                                        // Disconnect after discovery — lazy-connect on first real use
                                        let _ = client.cancel().await;

                                        resp.insert("tools_discovered".into(), json!(tools_count));
                                    }
                                    Err(e) => {
                                        resp.insert(
                                            "discovery_error".into(),
                                            json!(format!("list_tools failed: {e}")),
                                        );
                                    }
                                },
                                Err(e) => {
                                    resp.insert(
                                        "discovery_error".into(),
                                        json!(format!("MCP handshake failed: {e}")),
                                    );
                                }
                            },
                            Err(e) => {
                                resp.insert(
                                    "discovery_error".into(),
                                    json!(format!("spawn failed: {e}")),
                                );
                            }
                        }
                    }
                }
                "sse" => {
                    if let Some(url) = def["url"].as_str() {
                        let transport = StreamableHttpClientTransport::from_uri(url);
                        match rmcp::ServiceExt::serve((), transport).await {
                                Ok(client) => match client.peer().list_all_tools().await {
                                    Ok(tools) => {
                                        let tools_count = tools.len();

                                        self.proxy_tools
                                            .lock()
                                            .unwrap()
                                            .insert(server_name.to_string(), tools.clone());

                                        let mut updated_def = def.clone();
                                        updated_def["discovered_tools"] =
                                            serde_json::to_value(&tools).unwrap_or_default();
                                        updated_def["discovered_at"] = json!(Utc::now().to_rfc3339());
                                        updated_def["tools_count"] = json!(tools_count);

                                        let updated_cap = HubCapability {
                                            definition: serde_json::to_string(&updated_def)
                                                .unwrap_or(cap.definition.clone()),
                                            ..cap.clone()
                                        };
                                        let _ = self.with_store_for_scope(target_db, |store| {
                                            store
                                                .hub_register(&updated_cap)
                                                .map_err(|e| format!("update: {e}"))
                                        });

                                        // Disconnect after discovery — lazy-connect on first real use
                                        let _ = client.cancel().await;

                                        resp.insert("tools_discovered".into(), json!(tools_count));
                                    }
                                    Err(e) => {
                                        resp.insert(
                                            "discovery_error".into(),
                                            json!(format!("list_tools failed: {e}")),
                                        );
                                    }
                                },
                                Err(e) => {
                                    resp.insert(
                                        "discovery_error".into(),
                                        json!(format!("SSE handshake failed: {e}")),
                                    );
                                }
                            }
                    }
                }
                other => {
                    resp.insert(
                        "discovery_error".into(),
                        json!(format!("unsupported transport: {other}")),
                    );
                }
            }
        }

        serde_json::to_string(&serde_json::Value::Object(resp))
            .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Discover available capabilities (skills, plugins, MCP servers) in the Hub.")]
    async fn hub_discover(&self, Parameters(params): Parameters<HubDiscoverParams>) -> Result<String, String> {
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
    async fn hub_get(&self, Parameters(params): Parameters<HubGetParams>) -> Result<String, String> {
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
    async fn hub_feedback(&self, Parameters(params): Parameters<HubFeedbackParams>) -> Result<String, String> {
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
                return serde_json::to_string(&json!({"id": params.id, "recorded": true, "db": "project"}))
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
            store.hub_list(None, false).map_err(|e| format!("hub list: {e}"))
        })?;
        let project_caps = if self.project_db_path.is_some() {
            self.with_project_store(|store| {
                store.hub_list(None, false).map_err(|e| format!("hub list: {e}"))
            })?
        } else {
            vec![]
        };

        let total = global_caps.len() + project_caps.len();
        let mut by_type: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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

    #[tool(description = "Execute a registered Skill from the Hub using the internal LLM pipeline.")]
    async fn run_skill(&self, Parameters(params): Parameters<RunSkillParams>) -> Result<String, String> {
        let cap = {
            let mut found = None;
            if self.project_db_path.is_some() {
                found = self.with_project_store(|store| {
                    store.hub_get(&params.skill_id).map_err(|e| format!("hub get project: {e}"))
                })?;
            }
            if found.is_none() {
                found = self.with_global_store(|store| {
                    store.hub_get(&params.skill_id).map_err(|e| format!("hub get global: {e}"))
                })?;
            }
            found.ok_or_else(|| format!("Skill '{}' not found in Hub", params.skill_id))?
        };

        if cap.cap_type != "skill" {
            return Err(format!("'{}' is type '{}', not 'skill'", params.skill_id, cap.cap_type));
        }

        let def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| format!("invalid skill definition JSON: {e}"))?;

        let prompt_template = def["prompt"].as_str()
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

        self.llm.call_llm(
            "You are an AI assistant executing a specialized skill.",
            &resolved_prompt,
            None,
            0.3,
            4000
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

    #[tool(description = "Call a tool on a registered MCP server through the Hub using the shared connection pool.")]
    async fn hub_call(
        &self,
        Parameters(params): Parameters<HubCallParams>,
    ) -> Result<String, String> {
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
                serde_json::to_value(c).ok().and_then(|v| {
                    v.get("text").and_then(|t| t.as_str().map(String::from))
                })
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
            locks.entry(server_name.to_string())
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

        let cap = {
            let mut found = None;
            if self.project_db_path.is_some() {
                found = self
                    .with_project_store(|store| store.hub_get(&server_id).map_err(|e| format!("{e}")))
                    .unwrap_or(None);
            }
            if found.is_none() {
                found = self
                    .with_global_store(|store| store.hub_get(&server_id).map_err(|e| format!("{e}")))
                    .unwrap_or(None);
            }
            found.ok_or_else(|| {
                rmcp::ErrorData::invalid_params(format!("MCP server '{}' not found", server_id), None)
            })?
        };

        let def: serde_json::Value = serde_json::from_str(&cap.definition)
            .map_err(|e| rmcp::ErrorData::internal_error(format!("bad definition: {e}"), None))?;

        let transport_type = def["transport"].as_str().unwrap_or("stdio");

        let client = match transport_type {
            "stdio" => {
                let command = def["command"]
                    .as_str()
                    .ok_or_else(|| rmcp::ErrorData::internal_error("missing command", None))?;
                let args: Vec<String> = def["args"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let env_map = resolve_env_map(&def)
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;

                let mut cmd = tokio::process::Command::new(command);
                cmd.args(&args).env_clear();
                if let Ok(path) = std::env::var("PATH") {
                    cmd.env("PATH", &path);
                }
                if let Ok(home) = std::env::var("HOME") {
                    cmd.env("HOME", &home);
                }
                for (k, v) in &env_map {
                    cmd.env(k, v);
                }

                let transport = rmcp::transport::TokioChildProcess::new(cmd)
                    .map_err(|e| rmcp::ErrorData::internal_error(format!("spawn: {e}"), None))?;
                rmcp::ServiceExt::serve((), transport)
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(format!("handshake: {e}"), None))?
            }
            "sse" => {
                let url = def["url"]
                    .as_str()
                    .ok_or_else(|| rmcp::ErrorData::internal_error("missing url for SSE", None))?;
                let transport = StreamableHttpClientTransport::from_uri(url);
                rmcp::ServiceExt::serve((), transport)
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(format!("SSE handshake: {e}"), None))?
            }
            other => {
                return Err(rmcp::ErrorData::internal_error(
                    format!("unsupported transport: {other}"),
                    None,
                ));
            }
        };

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
        let cap_def = {
            let mut found = None;
            if self.project_db_path.is_some() {
                found = self.with_project_store(|store| {
                    store.hub_get(&server_id).map_err(|e| format!("{e}"))
                }).unwrap_or(None);
            }
            if found.is_none() {
                found = self.with_global_store(|store| {
                    store.hub_get(&server_id).map_err(|e| format!("{e}"))
                }).unwrap_or(None);
            }
            found.map(|c| serde_json::from_str::<serde_json::Value>(&c.definition).unwrap_or_default())
                .unwrap_or_default()
        };

        // 1. Check deny-list permissions
        if let Some(deny_list) = cap_def["permissions"]["deny"].as_array() {
            let denied: Vec<&str> = deny_list.iter().filter_map(|v| v.as_str()).collect();
            if denied.contains(&tool_name) {
                return Err(rmcp::ErrorData::invalid_params(
                    format!("Tool '{}' is denied by permissions policy on '{}'", tool_name, server_name),
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
            let needs_rebuild = sems.get(server_name)
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
        let _permit = semaphore.acquire().await
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
        ).await;

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
                Err(rmcp::ErrorData::internal_error(format!("proxy call failed: {e}"), None))
            }
            Err(_timeout) => {
                // Timeout — increment circuit breaker
                self.record_circuit_failure(server_name);
                Err(rmcp::ErrorData::internal_error(
                    format!("Tool call '{}' on '{}' timed out after {}ms", tool_name, server_name, timeout_ms),
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
            store.audit_log_insert(
                &timestamp, server_name, tool_name, &args_hash,
                success, duration_ms, error_kind.as_deref(),
            ).map_err(|e| format!("{e}"))
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
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        async move {
            let mut tools = self.tool_router.list_all();

            // Add proxy tools from registered MCP servers
            if let Ok(proxy) = self.proxy_tools.lock() {
                for (server_name, server_tools) in proxy.iter() {
                    for tool in server_tools {
                        let mut proxied = tool.clone();
                        proxied.name =
                            std::borrow::Cow::Owned(format!("{}__{}", server_name, tool.name));
                        tools.push(proxied);
                    }
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
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send + '_ {
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
                    self.proxy_call_internal(server_name, tool_name, params.arguments)
                        .await
                } else {
                    Err(rmcp::ErrorData::invalid_params("tool not found", None))
                }
            };

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

    let _ = dotenvy::from_path(home.join(".secrets/master.env"));
    let _ = dotenvy::from_path_override(app_home.join("config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".tachi/config.env"));
    // Backward compatibility with old Sigil paths
    let _ = dotenvy::from_path_override(home.join(".sigil/config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".sigil/config.env"));

    let git_root = find_git_root();

    // Resolve global DB path
    let global_db_path = if let Ok(p) = std::env::var("MEMORY_DB_PATH") {
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
    let project_db_path = if let Some(root) = git_root.as_ref() {
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
                let def: serde_json::Value = serde_json::from_str(&cap.definition).unwrap_or_default();
                if let Some(tools_json) = def.get("discovered_tools") {
                    if let Ok(tools) =
                        serde_json::from_value::<Vec<rmcp::model::Tool>>(tools_json.clone())
                    {
                        let server_name = cap.id.strip_prefix("mcp:").unwrap_or(&cap.id);
                        server
                            .proxy_tools
                            .lock()
                            .unwrap()
                            .insert(server_name.to_string(), tools);
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
    eprintln!("Transport: {}", if cli.daemon { format!("HTTP daemon on port {}", cli.port) } else { "stdio".to_string() });
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
            StreamableHttpServerConfig, StreamableHttpService,
            session::local::LocalSessionManager,
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
