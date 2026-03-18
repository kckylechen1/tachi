// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Replaces the Python mcp/server.py implementation.

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

mod llm;
mod prompts;

use chrono::Utc;
use memory_core::{MemoryEntry, MemoryStore, SearchOptions};
use rmcp::{
    model::{ServerInfo, ServerCapabilities, ToolsCapability},
    ServerHandler,
    tool,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::MutexGuard as StdMutexGuard;
use std::sync::Mutex as StdMutex;
use tokio::io::{stdin, stdout};
use tokio_util::sync::CancellationToken;

// ─── Server State ─────────────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
struct MemoryServer {
    store: Arc<StdMutex<MemoryStore>>,
    vec_available: bool,
    llm_client: llm::LlmClient,
}

impl MemoryServer {
    fn new(db_path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let store = MemoryStore::open(db_path.to_str().unwrap())?;
        let vec_available = store.vec_available;
        let llm_client = llm::LlmClient::new()?;
        Ok(Self {
            store: Arc::new(StdMutex::new(store)),
            vec_available,
            llm_client,
        })
    }

    fn lock_store(&self) -> Result<StdMutexGuard<'_, MemoryStore>, String> {
        self.store
            .lock()
            .map_err(|e| format!("memory store lock poisoned: {e}"))
    }

    fn prepare_shutdown(&self) {
        match self.lock_store() {
            Ok(store) => {
                if let Err(e) = store.prepare_shutdown() {
                    eprintln!("Failed to flush database on shutdown: {e}");
                }
            }
            Err(e) => eprintln!("Failed to lock database on shutdown: {e}"),
        }
    }
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
    "general".to_string()
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

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct IngestEventParams {
    /// Conversation identifier
    conversation_id: String,

    /// Turn identifier
    turn_id: String,

    /// Messages in the conversation turn
    messages: Vec<serde_json::Value>,
}

// ─── Tool Implementations ────────────────────────────────────────────────────────

#[tool(tool_box)]
impl MemoryServer {
    #[tool(description = "Save a memory entry to the store. Creates a new entry or updates an existing one if id is provided.")]
    async fn save_memory(
        &self,
        #[tool(aggr)] params: SaveMemoryParams,
    ) -> Result<String, String> {
        let id = params.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let timestamp = Utc::now().to_rfc3339();

        // Generate L0 summary if not provided
        let summary = if params.summary.is_empty() {
            self.llm_client.generate_summary(&params.text).await?
        } else {
            params.summary
        };

        // Generate embedding (do this BEFORE locking the store)
        let vector = self.llm_client.embed_voyage(&params.text, "document").await.ok();

        let entry = MemoryEntry {
            id: id.clone(),
            path: params.path,
            summary,
            text: params.text,
            importance: params.importance,
            timestamp,
            category: params.category,
            topic: params.topic,
            keywords: params.keywords,
            persons: params.persons,
            entities: params.entities,
            location: params.location,
            source: "mcp".to_string(),
            scope: params.scope,
            archived: false,
            access_count: 0,
            last_access: None,
            metadata: serde_json::json!({}),
            vector,
        };

        let mut store = self.lock_store()?;
        store.upsert(&entry)
            .map_err(|e| format!("Failed to save memory: {}", e))?;

        serde_json::to_string(&json!({
            "id": id,
            "timestamp": entry.timestamp
        })).map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Search memory entries using hybrid search (vector + FTS + symbolic). Returns ranked results with scores.")]
    async fn search_memory(
        &self,
        #[tool(aggr)] params: SearchMemoryParams,
    ) -> Result<String, String> {
        let mut opts = SearchOptions {
            top_k: params.top_k,
            path_prefix: params.path_prefix,
            include_archived: params.include_archived,
            candidates_per_channel: params.candidates_per_channel,
            mmr_threshold: params.mmr_threshold,
            graph_expand_hops: params.graph_expand_hops,
            graph_relation_filter: params.graph_relation_filter,
            ..Default::default()
        };
        opts.vec_available = self.vec_available;

        let mut store = self.lock_store()?;
        let results = store
            .search(&params.query, Some(opts))
            .map_err(|e| format!("Search failed: {}", e))?;

        // Format results as human-readable text like Python server
        let mut output = String::new();
        for (i, result) in results.iter().enumerate() {
            let score_percent = (result.score.final_score * 100.0) as u32;
            output.push_str(&format!(
                "{}. [id:{}] [{}] ({}%)\n   <Summary>: {}\n",
                i + 1,
                result.entry.id,
                result.entry.path,
                score_percent,
                result.entry.summary
            ));
        }
        Ok(output)
    }

    #[tool(description = "Get a single memory entry by ID.")]
    async fn get_memory(
        &self,
        #[tool(aggr)] params: GetMemoryParams,
    ) -> Result<String, String> {
        let store = self.lock_store()?;
        let entry = store
            .get_with_options(&params.id, params.include_archived)
            .map_err(|e| format!("Failed to get memory: {}", e))?;

        match entry {
            Some(e) => serde_json::to_string(&e)
                .map_err(|e| format!("Failed to serialize: {}", e)),
            None => {
                serde_json::to_string(&json!({
                    "error": "Memory not found"
                })).map_err(|e| format!("Failed to serialize: {}", e))
            }
        }
    }

    #[tool(description = "List memory entries under a path prefix.")]
    async fn list_memories(
        &self,
        #[tool(aggr)] params: ListMemoriesParams,
    ) -> Result<String, String> {
        let store = self.lock_store()?;
        let entries = store
            .list_by_path(&params.path_prefix, params.limit, params.include_archived)
            .map_err(|e| format!("Failed to list memories: {}", e))?;

        serde_json::to_string(&entries)
            .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(description = "Get aggregate statistics about the memory store.")]
    async fn memory_stats(&self) -> Result<String, String> {
        let store = self.lock_store()?;
        let stats = store
            .stats(false)
            .map_err(|e| format!("Failed to get stats: {}", e))?;

        serde_json::to_string(&stats)
            .map_err(|e| format!("Failed to serialize: {}", e))
    }

    #[tool(description = "Set a key-value pair in server state (stored in hard_state table).")]
    async fn set_state(
        &self,
        #[tool(aggr)] params: SetStateParams,
    ) -> Result<String, String> {
        let value_json = serde_json::to_string(&params.value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;

        let store = self.lock_store()?;
        let version = store.set_state("mcp", &params.key, &value_json)
            .map_err(|e| format!("Failed to set state: {}", e))?;

        serde_json::to_string(&json!({
            "key": params.key,
            "value": params.value,
            "version": version
        })).map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Get a value from server state by key.")]
    async fn get_state(
        &self,
        #[tool(aggr)] params: GetStateParams,
    ) -> Result<String, String> {
        let store = self.lock_store()?;

        match store.get_state_kv("mcp", &params.key) {
            Ok(Some((value, version))) => {
                // Parse the value JSON to return it as a proper JSON value
                let parsed_value: serde_json::Value = serde_json::from_str(&value)
                    .unwrap_or_else(|_| serde_json::json!(value));

                serde_json::to_string(&json!({
                    "key": params.key,
                    "value": parsed_value,
                    "version": version
                })).map_err(|e| format!("Failed to serialize: {}", e))
            }
            Ok(None) => {
                serde_json::to_string(&json!({
                    "key": params.key,
                    "error": "not found"
                })).map_err(|e| format!("Failed to serialize: {}", e))
            }
            Err(e) => Err(format!("Failed to get state: {}", e)),
        }
    }

    #[tool(description = "Extract structured facts from text using LLM and save to memory.")]
    async fn extract_facts(&self, #[tool(aggr)] params: ExtractFactsParams) -> Result<String, String> {
        let facts = self.llm_client.extract_facts(&params.text).await?;
        if facts.is_empty() {
            return Ok("No facts extracted.".to_string());
        }

        let mut store = self.lock_store()?;
        let mut saved = 0;

        for fact in &facts {
            let text = fact["text"].as_str().unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }

            let topic = fact["topic"].as_str().unwrap_or("").to_string();
            let importance = fact["importance"].as_f64().unwrap_or(0.7);
            let keywords: Vec<String> = fact["keywords"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let scope = fact["scope"].as_str().unwrap_or("general").to_string();
            let summary = text.chars().take(100).collect::<String>();

            let entry = MemoryEntry {
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
                source: "extraction".to_string(),
                scope,
                archived: false,
                access_count: 0,
                last_access: None,
                metadata: serde_json::json!({"source": params.source}),
                vector: None, // no embedding for extracted facts (too many API calls)
            };

            if store.upsert(&entry).is_ok() {
                saved += 1;
            }
        }

        Ok(format!("Extracted and saved {} facts from text.", saved))
    }

    #[tool(description = "Ingest a conversation event and extract facts from messages.")]
    async fn ingest_event(&self, #[tool(aggr)] params: IngestEventParams) -> Result<String, String> {
        // Concatenate message contents for fact extraction
        let combined_text: String = params
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<&str>>()
            .join("\n");

        if combined_text.trim().is_empty() {
            return Ok("No content to process.".to_string());
        }

        // Reuse extract_facts logic
        let facts = self.llm_client.extract_facts(&combined_text).await?;
        if facts.is_empty() {
            return Ok("No facts extracted from event.".to_string());
        }

        let mut store = self.lock_store()?;
        let mut saved = 0;

        for fact in &facts {
            let text = fact["text"].as_str().unwrap_or("").to_string();
            if text.is_empty() {
                continue;
            }

            let topic = fact["topic"].as_str().unwrap_or("").to_string();
            let importance = fact["importance"].as_f64().unwrap_or(0.7);
            let keywords: Vec<String> = fact["keywords"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let scope = fact["scope"].as_str().unwrap_or("general").to_string();
            let summary = text.chars().take(100).collect::<String>();

            let entry = MemoryEntry {
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
                source: format!("conversation:{}", params.conversation_id),
                scope,
                archived: false,
                access_count: 0,
                last_access: None,
                metadata: serde_json::json!({
                    "conversation_id": params.conversation_id,
                    "turn_id": params.turn_id,
                }),
                vector: None,
            };

            if store.upsert(&entry).is_ok() {
                saved += 1;
            }
        }

        Ok(format!("Ingested event: extracted and saved {} facts.", saved))
    }

    #[tool(description = "Get pipeline status and statistics.")]
    async fn get_pipeline_status(&self) -> Result<String, String> {
        let store = self.lock_store()?;

        // Get basic stats from the store
        let stats = store.stats(false).map_err(|e| format!("Failed to get stats: {}", e))?;

        serde_json::to_string(&json!({
            "status": "running",
            "workers": "rust_sync", // No async workers in Rust yet
            "total_entries": stats.total,
            "by_scope": stats.by_scope,
            "by_category": stats.by_category,
            "vec_available": self.vec_available,
        }))
        .map_err(|e| format!("Failed to serialize response: {}", e))
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let sigint = signal(SignalKind::interrupt());
    let sigterm = signal(SignalKind::terminate());

    if let (Ok(mut sigint), Ok(mut sigterm)) = (sigint, sigterm) {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ─── ServerHandler Implementation ────────────────────────────────────────────────

#[tool(tool_box)]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "A high-performance memory MCP server built with Rust. \
                Provides hybrid search (vector + FTS + symbolic), memory storage, and state management."
                    .into(),
            ),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::var("MEMORY_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut default = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            default.push(".sigil");
            default.push("memory.db");
            default
        });

    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // ── Single-instance guard via flock ──────────────────────────────────────
    // Prevents multiple server processes from writing to the same database,
    // which is a common cause of WAL corruption.
    let lock_path = db_path.with_extension("db.lock");
    let _lock_file = acquire_instance_lock(&lock_path)?;

    let server = MemoryServer::new(db_path.clone())?;

    // ── Startup integrity check ────────────────────────────────────────────
    {
        let store = server.lock_store().map_err(|e| format!("lock: {e}"))?;
        match store.quick_check() {
            Ok(true) => eprintln!("Database integrity: OK"),
            Ok(false) => eprintln!("WARNING: Database may be corrupted! Run PRAGMA integrity_check for details."),
            Err(e) => eprintln!("WARNING: Could not check database integrity: {e}"),
        }
    }

    let transport = (stdin(), stdout());

    eprintln!("Starting Memory MCP Server v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("Database path: {}", db_path.display());
    eprintln!("Vector search: {}", server.vec_available);
    eprintln!("Tools: save_memory, search_memory, get_memory, list_memories, memory_stats, set_state, get_state, extract_facts, ingest_event, get_pipeline_status");

    let shutdown_token = CancellationToken::new();
    let running = rmcp::service::serve_server_with_ct(server, transport, shutdown_token.clone()).await?;
    let shutdown_server = running.service().clone();
    let final_server = running.service().clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        eprintln!("Shutdown signal received, stopping Memory MCP Server...");
        shutdown_server.prepare_shutdown();
        shutdown_token.cancel();
    });

    let quit_reason = running.waiting().await?;
    final_server.prepare_shutdown();
    eprintln!("Memory MCP Server stopped: {:?}", quit_reason);

    Ok(())
}

/// Acquire an advisory file lock to detect multiple server instances.
/// Returns the lock File which must be kept alive for the process lifetime.
/// Multiple instances are allowed (busy_timeout handles coordination),
/// but a warning is emitted so users know about potential contention.
#[cfg(unix)]
fn acquire_instance_lock(path: &std::path::Path) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    // Try non-blocking exclusive lock — warn but don't fail
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        eprintln!(
            "WARNING: Another memory-server instance may be accessing this database (lock file: {}). \
             Concurrent access is supported via busy_timeout, but avoid heavy parallel writes.",
            path.display()
        );
    }

    // Write PID for debugging
    use std::io::Write;
    let mut f = &file;
    let _ = f.write_all(format!("{}", std::process::id()).as_bytes());
    let _ = f.flush();

    Ok(file)
}

#[cfg(not(unix))]
fn acquire_instance_lock(path: &std::path::Path) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    // On non-Unix, just create the file as a marker (no real locking)
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    eprintln!("WARNING: File locking not supported on this platform. Ensure only one instance runs.");
    Ok(file)
}
