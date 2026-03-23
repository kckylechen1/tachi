// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

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
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{stdin, stdout};

/// Build a slim JSON representation of a MemoryEntry for MCP output.
/// Strips internal fields (access_count, last_access, revision, source, vector)
/// and omits empty strings/arrays to minimize token usage.
fn slim_entry(e: &MemoryEntry) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(e.id));
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
fn slim_search_result(result: &memory_core::SearchResult) -> serde_json::Value {
    let mut obj = match slim_entry(&result.entry) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    // Round to 3 decimal places
    obj.insert("relevance".into(), json!((result.score.final_score * 1000.0).round() / 1000.0));
    serde_json::Value::Object(obj)
}

/// Build a slim L0 rule entry.
fn slim_l0_rule(rule: &MemoryEntry) -> serde_json::Value {
    let mut obj = match slim_entry(rule) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("l0_rule".into(), json!(true));
    serde_json::Value::Object(obj)
}

// ─── Server State ─────────────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
struct MemoryServer {
    db_path: Arc<PathBuf>,
    vec_available: bool,
    llm: Arc<llm::LlmClient>,
    pipeline_enabled: bool,
}

impl MemoryServer {
    fn new(db_path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        // Detect vec_available once at startup with a temporary connection
        let tmp_store = MemoryStore::open(db_path.to_str().unwrap())?;
        let vec_available = tmp_store.vec_available;
        drop(tmp_store);

        let llm = Arc::new(llm::LlmClient::new()?);
        let pipeline_enabled = std::env::var("ENABLE_PIPELINE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        Ok(Self {
            db_path: Arc::new(db_path),
            vec_available,
            llm,
            pipeline_enabled,
        })
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut MemoryStore) -> Result<T, String>) -> Result<T, String> {
        let mut store = MemoryStore::open(self.db_path.to_str().unwrap())
            .map_err(|e| format!("open store: {e}"))?;
        f(&mut store)
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

        // Generate L0 summary if not provided (async LLM work before DB open)
        let summary = if params.summary.is_empty() {
            self.llm.generate_summary(&params.text).await?
        } else {
            params.summary
        };

        // Generate embedding (async LLM work before DB open)
        let vector = self.llm.embed_voyage(&params.text, "document").await.ok();

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
            revision: 1,
            metadata: serde_json::json!({}),
            vector,
        };

        self.with_store(|store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory: {}", e))
        })?;

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

        let pipeline_enabled = self.pipeline_enabled;

        self.with_store(|store| {
            let results = store
                .search(&params.query, Some(opts))
                .map_err(|e| format!("Search failed: {}", e))?;
            let mut output: Vec<serde_json::Value> = results
                .iter()
                .map(|r| slim_search_result(r))
                .collect();

            if pipeline_enabled {
                let mut existing_ids: HashSet<String> = results
                    .iter()
                    .map(|r| r.entry.id.clone())
                    .collect();
                let rules = store
                    .list_by_path("/behavior/global_rules", 50, false)
                    .unwrap_or_default();
                for rule in rules {
                    if !is_active_global_rule(&rule) {
                        continue;
                    }
                    if !existing_ids.insert(rule.id.clone()) {
                        continue;
                    }
                    output.push(slim_l0_rule(&rule));
                }
            }

            serde_json::to_string(&output).map_err(|e| format!("Failed to serialize response: {}", e))
        })
    }

    #[tool(description = "Get a single memory entry by ID.")]
    async fn get_memory(
        &self,
        #[tool(aggr)] params: GetMemoryParams,
    ) -> Result<String, String> {
        self.with_store(|store| {
            let entry = store
                .get_with_options(&params.id, params.include_archived)
                .map_err(|e| format!("Failed to get memory: {}", e))?;

            match entry {
                Some(e) => serde_json::to_string(&slim_entry(&e))
                    .map_err(|e| format!("Failed to serialize: {}", e)),
                None => {
                    serde_json::to_string(&json!({
                        "error": "Memory not found"
                    })).map_err(|e| format!("Failed to serialize: {}", e))
                }
            }
        })
    }

    #[tool(description = "List memory entries under a path prefix.")]
    async fn list_memories(
        &self,
        #[tool(aggr)] params: ListMemoriesParams,
    ) -> Result<String, String> {
        self.with_store(|store| {
            let entries = store
                .list_by_path(&params.path_prefix, params.limit, params.include_archived)
                .map_err(|e| format!("Failed to list memories: {}", e))?;

            let slim: Vec<serde_json::Value> = entries.iter().map(|e| slim_entry(e)).collect();
            serde_json::to_string(&slim)
                .map_err(|e| format!("Failed to serialize: {}", e))
        })
    }

    #[tool(description = "Get aggregate statistics about the memory store.")]
    async fn memory_stats(&self) -> Result<String, String> {
        self.with_store(|store| {
            let stats = store
                .stats(false)
                .map_err(|e| format!("Failed to get stats: {}", e))?;

            serde_json::to_string(&stats)
                .map_err(|e| format!("Failed to serialize: {}", e))
        })
    }

    #[tool(description = "Set a key-value pair in server state (stored in hard_state table).")]
    async fn set_state(
        &self,
        #[tool(aggr)] params: SetStateParams,
    ) -> Result<String, String> {
        let value_json = serde_json::to_string(&params.value)
            .map_err(|e| format!("Failed to serialize value: {}", e))?;

        self.with_store(|store| {
            let version = store.set_state("mcp", &params.key, &value_json)
                .map_err(|e| format!("Failed to set state: {}", e))?;

            serde_json::to_string(&json!({
                "key": params.key,
                "value": params.value,
                "version": version
            })).map_err(|e| format!("Failed to serialize response: {}", e))
        })
    }

    #[tool(description = "Get a value from server state by key.")]
    async fn get_state(
        &self,
        #[tool(aggr)] params: GetStateParams,
    ) -> Result<String, String> {
        self.with_store(|store| {
            match store.get_state_kv("mcp", &params.key) {
                Ok(Some((value, version))) => {
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
        })
    }

    #[tool(description = "Extract structured facts from text using LLM and save to memory.")]
    async fn extract_facts(&self, #[tool(aggr)] params: ExtractFactsParams) -> Result<String, String> {
        // Do async LLM work before opening DB
        let facts = self.llm.extract_facts(&params.text).await?;
        if facts.is_empty() {
            return Ok("No facts extracted.".to_string());
        }

        self.with_store(|store| {
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
                    revision: 1,
                    metadata: serde_json::json!({"source": params.source}),
                    vector: None,
                };

                if store.upsert(&entry).is_ok() {
                    saved += 1;
                }
            }

            Ok(format!("Extracted and saved {} facts from text.", saved))
        })
    }

    #[tool(description = "Ingest a conversation event and extract facts from messages.")]
    async fn ingest_event(&self, #[tool(aggr)] params: IngestEventParams) -> Result<String, String> {
        let mut hasher = DefaultHasher::new();
        params.conversation_id.hash(&mut hasher);
        params.turn_id.hash(&mut hasher);
        let event_hash = format!("{:x}", hasher.finish());

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

        // Check dedup with per-request DB open
        let already_processed = self.with_store(|store| {
            store
                .is_event_processed(&event_hash, "ingest")
                .map_err(|e| format!("Failed to check event dedup: {e}"))
        })?;
        if already_processed {
            return Ok(format!("Event already processed (hash: {})", event_hash));
        }

        // Do async LLM work before opening DB for writes
        let facts = self.llm.extract_facts(&combined_text).await?;
        let mut saved = 0;

        self.with_store(|store| {
            if !facts.is_empty() {
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
                        revision: 1,
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
            }

            store
                .mark_event_processed(
                    &event_hash,
                    &format!("{}:{}", params.conversation_id, params.turn_id),
                    "ingest",
                )
                .map_err(|e| format!("Failed to mark event processed: {e}"))?;

            Ok(())
        })?;

        if facts.is_empty() {
            Ok("No facts extracted from event.".to_string())
        } else {
            Ok(format!("Ingested event: extracted and saved {} facts.", saved))
        }
    }

    #[tool(description = "Get pipeline status and statistics.")]
    async fn get_pipeline_status(&self) -> Result<String, String> {
        self.with_store(|store| {
            let stats = store.stats(false).map_err(|e| format!("Failed to get stats: {}", e))?;

            serde_json::to_string(&json!({
                "status": "running",
                "workers": if self.pipeline_enabled { "rust_async" } else { "disabled" },
                "total_entries": stats.total,
                "by_scope": stats.by_scope,
                "by_category": stats.by_category,
                "vec_available": self.vec_available,
                "pipeline_enabled": self.pipeline_enabled,
            }))
            .map_err(|e| format!("Failed to serialize response: {}", e))
        })
    }
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
    // Load config from dotenv files (low→high priority, later wins):
    //   1. ~/.secrets/master.env      (shared API keys)
    //   2. ~/.sigil/config.env        (user-level defaults)
    //   3. .sigil/config.env          (project-level overrides, e.g. MEMORY_DB_PATH)
    //   4. Process env vars           (MCP harness overrides, highest priority)
    // dotenvy::from_path_override means later files override earlier ones,
    // but process env vars set before launch always win.
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let _ = dotenvy::from_path(home.join(".secrets/master.env"));
    let _ = dotenvy::from_path_override(home.join(".sigil/config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".sigil/config.env"));

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

    let server = MemoryServer::new(db_path.clone())?;

    // ── Startup integrity check (per-request open) ─────────────────────────
    server.with_store(|store| {
        match store.quick_check() {
            Ok(true) => eprintln!("Database integrity: OK"),
            Ok(false) => eprintln!("WARNING: Database may be corrupted! Run PRAGMA integrity_check for details."),
            Err(e) => eprintln!("WARNING: Could not check database integrity: {e}"),
        }
        Ok(())
    }).map_err(|e| format!("startup check: {e}"))?;

    if server.pipeline_enabled {
        eprintln!("Pipeline workers: ENABLED (external)");
    } else {
        eprintln!("Pipeline workers: DISABLED (set ENABLE_PIPELINE=true to enable)");
    }

    let transport = (stdin(), stdout());

    eprintln!("Starting Memory MCP Server v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("Database path: {}", db_path.display());
    eprintln!("Vector search: {}", server.vec_available);
    eprintln!("Tools: save_memory, search_memory, get_memory, list_memories, memory_stats, set_state, get_state, extract_facts, ingest_event, get_pipeline_status");

    let running = rmcp::service::serve_server(server, transport).await?;
    let quit_reason = running.waiting().await?;

    eprintln!("Memory MCP Server stopped: {:?}", quit_reason);

    Ok(())
}
