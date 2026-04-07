// lib.rs — Public API for memory-core
//
// Re-exports all primary types and provides a MemoryStore handle that
// bundles a rusqlite::Connection with convenience methods.

pub mod db;
pub mod error;
pub mod hub;
pub mod noise;
pub mod scorer;
pub mod search;
pub mod types;

pub use error::MemoryError;
pub use hub::HubCapability;
pub use types::{HybridScore, MemoryEntry, MemoryEdge, GraphExpandResult, SearchResult, StatsResult};
pub use search::{SearchOptions, hybrid_search};
pub use scorer::HybridWeights;
pub use noise::{is_noise_text, should_skip_query};

use rusqlite::Connection;
use std::time::Duration;

/// High-level handle that owns a database connection.
/// Language bindings (NAPI, PyO3) will wrap this struct.
pub struct MemoryStore {
    conn: Connection,
    pub vec_available: bool,
}

impl MemoryStore {
    /// Open (or create) a memory database at the given path.
    pub fn open(db_path: &str) -> Result<Self, MemoryError> {
        // Register extensions BEFORE opening the connection.
        libsimple::enable_auto_extension()
            .map_err(|e| MemoryError::InvalidArg(format!("simple tokenizer init: {e}")))?;
        db::register_sqlite_vec();
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        db::init_schema(&conn)?;
        let vec_available = db::try_load_sqlite_vec(&conn);
        Ok(Self { conn, vec_available })
    }

    /// In-memory database (useful for tests and scripts).
    pub fn open_in_memory() -> Result<Self, MemoryError> {
        libsimple::enable_auto_extension()
            .map_err(|e| MemoryError::InvalidArg(format!("simple tokenizer init: {e}")))?;
        db::register_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        db::init_schema(&conn)?;
        let vec_available = db::try_load_sqlite_vec(&conn);
        Ok(Self { conn, vec_available })
    }

    /// Insert or update a memory entry (with optional embedding vector).
    pub fn upsert(&mut self, entry: &MemoryEntry) -> Result<(), MemoryError> {
        db::upsert(&mut self.conn, entry, self.vec_available)
    }

    /// Check whether an event hash has already been processed by a worker.
    pub fn is_event_processed(&self, event_hash: &str, worker: &str) -> Result<bool, MemoryError> {
        db::is_event_processed(&self.conn, event_hash, worker)
    }

    /// Mark an event hash as processed by a worker.
    pub fn mark_event_processed(&self, event_hash: &str, event_id: &str, worker: &str) -> Result<(), MemoryError> {
        db::mark_event_processed(&self.conn, event_hash, event_id, worker)
    }

    /// Atomically try to claim an event for processing.
    /// Returns true if claimed (first processor), false if already processed.
    pub fn try_claim_event(&self, event_hash: &str, event_id: &str, worker: &str) -> Result<bool, MemoryError> {
        db::try_claim_event(&self.conn, event_hash, event_id, worker)
    }

    /// Release a claimed event on processing failure (at-least-once delivery).
    pub fn release_event_claim(&self, event_hash: &str, worker: &str) -> Result<(), MemoryError> {
        db::release_event_claim(&self.conn, event_hash, worker)
    }

    /// Revision-checked update used for optimistic locking in merge flows.
    pub fn update_with_revision(
        &mut self,
        id: &str,
        new_text: &str,
        new_summary: &str,
        new_source: &str,
        new_metadata: &serde_json::Value,
        new_vec: Option<&[f32]>,
        expected_revision: i64,
    ) -> Result<bool, MemoryError> {
        let metadata_json = serde_json::to_string(new_metadata)?;
        let vec_blob = if self.vec_available {
            new_vec.map(db::serialize_f32)
        } else {
            None
        };

        db::update_with_revision(
            &mut self.conn,
            id,
            new_text,
            new_summary,
            new_source,
            &metadata_json,
            vec_blob.as_deref(),
            expected_revision,
        )
    }

    /// Update only enrichment fields (summary + vector) with revision check.
    /// Returns false if revision mismatch (entry was updated since enrichment started).
    pub fn update_enrichment_fields(
        &mut self,
        id: &str,
        new_summary: Option<&str>,
        new_vec: Option<&[f32]>,
        expected_revision: i64,
    ) -> Result<bool, MemoryError> {
        let vec_blob = if self.vec_available {
            new_vec.map(db::serialize_f32)
        } else {
            None
        };
        db::update_enrichment_fields(
            &mut self.conn,
            id,
            new_summary,
            vec_blob.as_deref(),
            expected_revision,
        )
    }

    /// Hybrid search: Text + FTS5 + optional vector channel.
    pub fn search(
        &mut self,
        query: &str,
        opts: Option<SearchOptions>,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let mut options = opts.unwrap_or_default();
        options.vec_available = self.vec_available;
        hybrid_search(&mut self.conn, query, &options)
    }

    /// Fetch a single entry by ID.
    pub fn get(&self, id: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        self.get_with_options(id, false)
    }

    /// Fetch a single entry by ID with archive visibility control.
    pub fn get_with_options(
        &self,
        id: &str,
        include_archived: bool,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let ids = vec![id.to_string()];
        let mut map = db::fetch_by_ids(&self.conn, &ids, include_archived)?;
        Ok(map.remove(id))
    }

    /// Low-level access for bindings that need raw Connection reference.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Fetch multiple newest entries up to a limit (used for dedup).
    pub fn get_all(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.get_all_with_options(limit, false)
    }

    /// Fetch newest entries with archive visibility control.
    pub fn get_all_with_options(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        db::get_all(&self.conn, limit, include_archived)
    }

    /// List entries under a path (exact + descendants) with SQL pushdown.
    pub fn list_by_path(
        &self,
        path_prefix: &str,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        db::list_by_path(&self.conn, path_prefix, limit, include_archived)
    }

    /// Delete a memory entry by ID. Returns true if found and deleted.
    pub fn delete(&mut self, id: &str) -> Result<bool, MemoryError> {
        db::delete(&mut self.conn, id, self.vec_available)
    }

    /// Run PRAGMA quick_check to detect database corruption early.
    /// Returns Ok(true) if healthy, Ok(false) if corrupt.
    pub fn quick_check(&self) -> Result<bool, MemoryError> {
        let result: String = self.conn.query_row(
            "PRAGMA quick_check",
            [],
            |row| row.get(0),
        )?;
        Ok(result == "ok")
    }

    /// Flush SQLite state before process shutdown.
    pub fn prepare_shutdown(&self) -> Result<(), MemoryError> {
        self.conn.execute_batch(
            "PRAGMA optimize;\nPRAGMA wal_checkpoint(PASSIVE);",
        )?;
        Ok(())
    }

    /// Get aggregate statistics about the memory store.
    pub fn stats(&self, include_archived: bool) -> Result<StatsResult, MemoryError> {
        db::stats(&self.conn, include_archived)
    }

    // ─── Graph Operations ────────────────────────────────────────────────────

    /// Add or update an edge in the memory graph.
    pub fn add_edge(&self, edge: &MemoryEdge) -> Result<(), MemoryError> {
        db::add_edge(&self.conn, edge)
    }

    /// Remove a specific edge.
    pub fn remove_edge(
        &self,
        source_id: &str,
        target_id: &str,
        relation: &str,
    ) -> Result<bool, MemoryError> {
        db::remove_edge(&self.conn, source_id, target_id, relation)
    }

    /// Get edges connected to a memory entry.
    pub fn get_edges(
        &self,
        memory_id: &str,
        direction: &str,
        relation_filter: Option<&str>,
    ) -> Result<Vec<MemoryEdge>, MemoryError> {
        db::get_edges(&self.conn, memory_id, direction, relation_filter)
    }

    /// BFS expansion from seed IDs through the memory graph.
    pub fn graph_expand(
        &self,
        seed_ids: &[String],
        max_hops: u32,
        relation_filter: Option<&str>,
    ) -> Result<GraphExpandResult, MemoryError> {
        db::graph_expand(&self.conn, seed_ids, max_hops, relation_filter)
    }

    // ─── Derived Items Operations ──────────────────────────────────────────────

    /// Save a derived item (causal extraction, distilled rule, etc.)
    pub fn save_derived(
        &self,
        text: &str,
        path: &str,
        summary: &str,
        importance: f64,
        source: &str,
        scope: &str,
        metadata: &serde_json::Value,
    ) -> Result<String, MemoryError> {
        db::save_derived(&self.conn, text, path, summary, importance, source, scope, metadata)
    }

    /// Count derived items by source and path prefix.
    pub fn count_derived_by_source(&self, source: &str, path_prefix: &str) -> Result<u64, MemoryError> {
        db::count_derived_by_source(&self.conn, source, path_prefix)
    }

    /// List derived items by source and path prefix.
    pub fn list_derived_by_source(
        &self,
        source: &str,
        path_prefix: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, MemoryError> {
        db::list_derived_by_source(&self.conn, source, path_prefix, limit)
    }

    /// Archive a memory entry (set archived=1, used after merge).
    pub fn archive_memory(&self, id: &str) -> Result<bool, MemoryError> {
        db::archive_memory(&self.conn, id)
    }

    /// Run retention-based garbage collection on growing tables.
    /// Prunes access_history (keep 256/memory), processed_events (30d),
    /// audit_log (30d + 100k cap), agent_known_state (90d), and orphans.
    pub fn gc_tables(&mut self) -> Result<serde_json::Value, MemoryError> {
        db::gc_tables(&mut self.conn)
    }

    // ─── Hard State Operations ────────────────────────────────────────────────

    /// Set a deterministic key-value state.
    pub fn set_state(&self, namespace: &str, key: &str, value_json: &str) -> Result<u32, MemoryError> {
        db::set_state(&self.conn, namespace, key, value_json)
    }

    /// Get a deterministic key-value state.
    pub fn get_state_kv(&self, namespace: &str, key: &str) -> Result<Option<(String, u32)>, MemoryError> {
        db::get_state(&self.conn, namespace, key)
    }

    // ─── Hub Operations ──────────────────────────────────────────────────────

    /// Register or update a hub capability (skill, plugin, or MCP server).
    pub fn hub_register(&self, cap: &HubCapability) -> Result<(), MemoryError> {
        db::hub_upsert(&self.conn, cap)
    }

    /// Get a single hub capability by ID.
    pub fn hub_get(&self, id: &str) -> Result<Option<HubCapability>, MemoryError> {
        db::hub_get(&self.conn, id)
    }

    /// List hub capabilities, optionally filtered by type and enabled status.
    pub fn hub_list(&self, cap_type: Option<&str>, enabled_only: bool) -> Result<Vec<HubCapability>, MemoryError> {
        db::hub_list(&self.conn, cap_type, enabled_only)
    }

    /// Search hub capabilities by name/description.
    pub fn hub_search(&self, query: &str, cap_type: Option<&str>) -> Result<Vec<HubCapability>, MemoryError> {
        db::hub_search(&self.conn, query, cap_type)
    }

    /// Enable or disable a hub capability.
    pub fn hub_set_enabled(&self, id: &str, enabled: bool) -> Result<bool, MemoryError> {
        db::hub_set_enabled(&self.conn, id, enabled)
    }

    /// Record feedback for a hub capability invocation.
    pub fn hub_record_feedback(&self, id: &str, success: bool, rating: Option<f64>) -> Result<bool, MemoryError> {
        db::hub_record_feedback(&self.conn, id, success, rating)
    }

    /// Delete a hub capability.
    pub fn hub_delete(&self, id: &str) -> Result<bool, MemoryError> {
        db::hub_delete(&self.conn, id)
    }

    // ─── Audit Log Operations ────────────────────────────────────────────────

    /// Insert an audit log entry.
    pub fn audit_log_insert(
        &self,
        timestamp: &str,
        server_id: &str,
        tool_name: &str,
        args_hash: &str,
        success: bool,
        duration_ms: u64,
        error_kind: Option<&str>,
    ) -> Result<(), MemoryError> {
        db::audit_log_insert(&self.conn, timestamp, server_id, tool_name, args_hash, success, duration_ms, error_kind)
    }

    /// List recent audit log entries.
    pub fn audit_log_list(&self, limit: usize, server_filter: Option<&str>) -> Result<Vec<serde_json::Value>, MemoryError> {
        db::audit_log_list(&self.conn, limit, server_filter)
    }

    // ─── Agent Known State (Context Diffing) ─────────────────────────────────

    /// Get the known revisions for a set of memory IDs for a given agent.
    pub fn get_agent_known_revisions(
        &self,
        agent_id: &str,
        memory_ids: &[String],
    ) -> Result<std::collections::HashMap<String, i64>, MemoryError> {
        db::get_agent_known_revisions(&self.conn, agent_id, memory_ids)
    }

    /// Update the agent's known state for a set of memory entries.
    pub fn update_agent_known_state(
        &self,
        agent_id: &str,
        entries: &[(String, i64)],
    ) -> Result<(), MemoryError> {
        db::update_agent_known_state(&self.conn, agent_id, entries)
    }

    // ─── Semantic Sandboxing ─────────────────────────────────────────────────

    /// Set a sandbox access rule for an agent role + path pattern.
    pub fn set_sandbox_rule(
        &self,
        agent_role: &str,
        path_pattern: &str,
        access_level: &str,
    ) -> Result<(), MemoryError> {
        db::set_sandbox_rule(&self.conn, agent_role, path_pattern, access_level)
    }

    /// Check if an agent role can access a path for a given operation.
    /// Returns (allowed, matching_rule_description).
    /// Advisory mode — not enforced in search_memory yet.
    // TODO: Integrate enforcement into search_memory by filtering results based on agent role
    pub fn check_sandbox_access(
        &self,
        agent_role: &str,
        path: &str,
        operation: &str,
    ) -> Result<(bool, Option<String>), MemoryError> {
        db::check_sandbox_access(&self.conn, agent_role, path, operation)
    }
}
