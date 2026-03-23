// main.rs — Memory MCP Server
//
// Rust MCP server using rmcp SDK to expose memory-core functionality.
// Stateless design: each tool opens its own DB connection per-request.

mod llm;
mod prompts;

use chrono::Utc;
use memory_core::{HubCapability, MemoryEntry, MemoryStore, SearchOptions};
use rmcp::{
    model::{ServerCapabilities, ServerInfo, ToolsCapability},
    tool,
    ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{stdin, stdout};

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

#[derive(Clone)]
#[allow(dead_code)]
struct MemoryServer {
    global_db_path: Arc<PathBuf>,
    project_db_path: Option<Arc<PathBuf>>,
    global_vec_available: bool,
    project_vec_available: bool,
    llm: Arc<llm::LlmClient>,
    pipeline_enabled: bool,
}

impl MemoryServer {
    fn new(
        global_db_path: PathBuf,
        project_db_path: Option<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Probe vec_available on global DB
        let tmp = MemoryStore::open(global_db_path.to_str().unwrap())?;
        let global_vec_available = tmp.vec_available;
        drop(tmp);

        // Probe vec_available on project DB if present
        let (project_db_path, project_vec_available) = if let Some(ref p) = project_db_path {
            let tmp = MemoryStore::open(p.to_str().unwrap())?;
            let v = tmp.vec_available;
            drop(tmp);
            (Some(Arc::new(p.clone())), v)
        } else {
            (None, false)
        };

        let llm = Arc::new(llm::LlmClient::new()?);
        let pipeline_enabled = std::env::var("ENABLE_PIPELINE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        Ok(Self {
            global_db_path: Arc::new(global_db_path),
            project_db_path,
            global_vec_available,
            project_vec_available,
            llm,
            pipeline_enabled,
        })
    }

    fn with_global_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut store = MemoryStore::open(self.global_db_path.to_str().unwrap())
            .map_err(|e| format!("open global store: {e}"))?;
        f(&mut store)
    }

    fn with_project_store<T>(
        &self,
        f: impl FnOnce(&mut MemoryStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let path = self
            .project_db_path
            .as_ref()
            .ok_or_else(|| "No project database available (not in a git repository)".to_string())?;
        let mut store = MemoryStore::open(path.to_str().unwrap())
            .map_err(|e| format!("open project store: {e}"))?;
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
        let requested_scope = params.scope.clone();
        let (target_db, warning) = self.resolve_write_scope(&requested_scope);

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
            scope: requested_scope,
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            metadata: serde_json::json!({}),
            vector,
        };

        self.with_store_for_scope(target_db, |store| {
            store
                .upsert(&entry)
                .map_err(|e| format!("Failed to save memory: {}", e))
        })?;

        let mut response = serde_json::Map::new();
        response.insert("id".into(), json!(id));
        response.insert("timestamp".into(), json!(entry.timestamp));
        response.insert("db".into(), json!(target_db.as_str()));
        if let Some(warning) = warning {
            response.insert("warning".into(), json!(warning));
        }

        serde_json::to_string(&serde_json::Value::Object(response))
            .map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Search memory entries using hybrid search (vector + FTS + symbolic). Returns ranked results with scores.")]
    async fn search_memory(
        &self,
        #[tool(aggr)] params: SearchMemoryParams,
    ) -> Result<String, String> {
        let pipeline_enabled = self.pipeline_enabled;

        let mut combined_results: Vec<(memory_core::SearchResult, DbScope)> = Vec::new();

        let mut global_opts = SearchOptions {
            top_k: params.top_k,
            path_prefix: params.path_prefix.clone(),
            include_archived: params.include_archived,
            candidates_per_channel: params.candidates_per_channel,
            mmr_threshold: params.mmr_threshold,
            graph_expand_hops: params.graph_expand_hops,
            graph_relation_filter: params.graph_relation_filter.clone(),
            ..Default::default()
        };
        global_opts.vec_available = self.global_vec_available;

        let global_results = self.with_global_store(|store| {
            store
                .search(&params.query, Some(global_opts))
                .map_err(|e| format!("Search failed in global DB: {}", e))
        })?;
        combined_results.extend(global_results.into_iter().map(|r| (r, DbScope::Global)));

        if self.project_db_path.is_some() {
            let mut project_opts = SearchOptions {
                top_k: params.top_k,
                path_prefix: params.path_prefix.clone(),
                include_archived: params.include_archived,
                candidates_per_channel: params.candidates_per_channel,
                mmr_threshold: params.mmr_threshold,
                graph_expand_hops: params.graph_expand_hops,
                graph_relation_filter: params.graph_relation_filter.clone(),
                ..Default::default()
            };
            project_opts.vec_available = self.project_vec_available;

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
        #[tool(aggr)] params: GetMemoryParams,
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
        #[tool(aggr)] params: ListMemoriesParams,
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
        #[tool(aggr)] params: SetStateParams,
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
        #[tool(aggr)] params: GetStateParams,
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
    async fn extract_facts(&self, #[tool(aggr)] params: ExtractFactsParams) -> Result<String, String> {
        // Do async LLM work before opening DB
        let facts = self.llm.extract_facts(&params.text).await?;
        if facts.is_empty() {
            return Ok("No facts extracted.".to_string());
        }

        let (target_db, _warning) = self.resolve_write_scope("project");

        self.with_store_for_scope(target_db, |store| {
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

        // Check dedup with per-request DB open
        let already_processed = self.with_store_for_scope(target_db, |store| {
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

        self.with_store_for_scope(target_db, |store| {
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
        }))
        .map_err(|e| format!("Failed to serialize response: {}", e))
    }

    #[tool(description = "Register a capability (skill, plugin, or MCP server) in the Hub.")]
    async fn hub_register(&self, #[tool(aggr)] params: HubRegisterParams) -> Result<String, String> {
        let (target_db, warning) = self.resolve_write_scope(&params.scope);
        let cap = HubCapability {
            id: params.id.clone(),
            cap_type: params.cap_type,
            name: params.name,
            version: params.version,
            description: params.description,
            definition: params.definition,
            enabled: true,
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
        serde_json::to_string(&serde_json::Value::Object(resp))
            .map_err(|e| format!("serialize: {e}"))
    }

    #[tool(description = "Discover available capabilities (skills, plugins, MCP servers) in the Hub.")]
    async fn hub_discover(&self, #[tool(aggr)] params: HubDiscoverParams) -> Result<String, String> {
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
    async fn hub_get(&self, #[tool(aggr)] params: HubGetParams) -> Result<String, String> {
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
    async fn hub_feedback(&self, #[tool(aggr)] params: HubFeedbackParams) -> Result<String, String> {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("sigil {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    if let Err(e) = tokio_main() {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn tokio_main() -> Result<(), Box<dyn std::error::Error>> {
    // Load config from dotenv files (same as before)
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let _ = dotenvy::from_path(home.join(".secrets/master.env"));
    let _ = dotenvy::from_path_override(home.join(".sigil/config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".sigil/config.env"));

    let git_root = find_git_root();

    // Resolve global DB path
    let global_db_path = if let Ok(p) = std::env::var("MEMORY_DB_PATH") {
        PathBuf::from(p)
    } else {
        let default_global = home.join(".sigil/global/memory.db");
        // Migration: move legacy ~/.sigil/memory.db → ~/.sigil/global/memory.db
        let legacy = home.join(".sigil/memory.db");
        if legacy.exists() && !default_global.exists() {
            if let Some(parent) = default_global.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&legacy, &default_global).await?;
            eprintln!(
                "Migrated legacy DB: {} → {}",
                legacy.display(),
                default_global.display()
            );
        }
        default_global
    };

    // Resolve project DB path
    let project_db_path = git_root.as_ref().map(|root| root.join(".sigil/memory.db"));

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

    if server.pipeline_enabled {
        eprintln!("Pipeline workers: ENABLED (external)");
    } else {
        eprintln!("Pipeline workers: DISABLED (set ENABLE_PIPELINE=true to enable)");
    }

    let transport = (stdin(), stdout());

    eprintln!("Starting Memory MCP Server v{}", env!("CARGO_PKG_VERSION"));
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
    eprintln!("Tools: save_memory, search_memory, get_memory, list_memories, memory_stats, set_state, get_state, extract_facts, ingest_event, get_pipeline_status, hub_register, hub_discover, hub_get, hub_feedback, hub_stats");

    let running = rmcp::service::serve_server(server, transport).await?;
    let quit_reason = running.waiting().await?;

    eprintln!("Memory MCP Server stopped: {:?}", quit_reason);

    Ok(())
}
