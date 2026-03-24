//! memory_core_py — PyO3 binding for memory-core.
//!
//! Exposes MemoryStore to Python with search() and upsert() methods.
//! All SQLite + scoring logic runs in Rust; LLM calls (Voyage, GLM-5) stay in Python.
//!
//! Usage:
//!     from memory_core_py import MemoryStore
//!     store = MemoryStore("/path/to/memory.db")
//!     store.upsert({"id": "...", "text": "...", ...})
//!     results = store.search("query text", {"top_k": 6})
use pyo3::prelude::*;
use memory_core::{
    is_noise_text,
    should_skip_query,
    MemoryEntry,
    MemoryStore as RustStore,
    SearchOptions,
};

use std::sync::{Arc, Mutex};

// ── Python MemoryStore class ──────────────────────────────────────────────────

#[pyclass(name = "MemoryStore")]
struct PyMemoryStore {
    inner: Arc<Mutex<RustStore>>,
}

#[pymethods]
impl PyMemoryStore {
    /// Open the store at `db_path`.
    #[new]
    fn new(db_path: &str) -> PyResult<Self> {
        let store = RustStore::open(db_path)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { inner: Arc::new(Mutex::new(store)) })
    }

    /// Upsert a memory entry. `entry_json` is a JSON string.
    fn upsert(&self, entry_json: &str) -> PyResult<()> {
        let me: MemoryEntry = serde_json::from_str(entry_json)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        let mut store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        store.upsert(&me)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Hybrid search. Returns a JSON string of results.
    #[pyo3(signature = (query, options_json=None))]
    fn search(
        &self,
        query: &str,
        options_json: Option<&str>,
    ) -> PyResult<String> {
        let mut opts = SearchOptions::default();
        if let Some(json_str) = options_json {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(n) = val.get("top_k").and_then(|v| v.as_u64()) {
                    opts.top_k = n as usize;
                }
                if let Some(n) = val.get("candidates").and_then(|v| v.as_u64()) {
                    opts.candidates_per_channel = n as usize;
                }
                if let Some(s) = val.get("path_prefix").and_then(|v| v.as_str()) {
                    opts.path_prefix = Some(s.to_string());
                }
                if let Some(b) = val.get("record_access").and_then(|v| v.as_bool()) {
                    opts.record_access = b;
                }
                if let Some(b) = val.get("include_archived").and_then(|v| v.as_bool()) {
                    opts.include_archived = b;
                }
                if val.get("mmr_threshold").is_some() {
                    opts.mmr_threshold = val.get("mmr_threshold").and_then(|v| v.as_f64());
                }
                if let Some(arr) = val.get("query_vec").and_then(|v| v.as_array()) {
                    let mut qv = Vec::with_capacity(arr.len());
                    for item in arr {
                        if let Some(num) = item.as_f64() {
                            qv.push(num as f32);
                        }
                    }
                    if !qv.is_empty() {
                        opts.query_vec = Some(qv);
                    }
                }
                if let Some(w) = val.get("weights").and_then(|v| v.as_object()) {
                    if let Some(s) = w.get("semantic").and_then(|v| v.as_f64()) {
                        opts.weights.semantic = s;
                    }
                    if let Some(f) = w.get("fts").and_then(|v| v.as_f64()) {
                        opts.weights.fts = f;
                    }
                    if let Some(s) = w.get("symbolic").and_then(|v| v.as_f64()) {
                        opts.weights.symbolic = s;
                    }
                    if let Some(d) = w.get("decay").and_then(|v| v.as_f64()) {
                        opts.weights.decay = d;
                    }
                }
            }
        }

        let mut store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let results = store
            .search(query, Some(opts))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&results)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Delete a memory by ID. Returns true if deleted, false if not found.
    #[pyo3(signature = (id,))]
    fn delete(&self, id: &str) -> PyResult<bool> {
        let mut store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        store.delete(id)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Get aggregate stats. Returns JSON string of StatsResult.
    #[pyo3(signature = (include_archived=false))]
    fn stats(&self, include_archived: bool) -> PyResult<String> {
        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stats = store
            .stats(include_archived)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&stats)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Check if text is noise (should not be stored). Pure Rust, no I/O.
    #[staticmethod]
    fn is_noise(text: &str) -> bool {
        is_noise_text(text)
    }

    /// Check if query should skip retrieval. Pure Rust, no I/O.
    #[staticmethod]
    fn should_skip(query: &str) -> bool {
        should_skip_query(query)
    }

    /// Fetch a single memory by ID. Returns JSON string or None.
    #[pyo3(signature = (id, include_archived=false))]
    fn get(&self, id: &str, include_archived: bool) -> PyResult<Option<String>> {
        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = store
            .get_with_options(id, include_archived)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        match entry {
            Some(e) => {
                let s = serde_json::to_string(&e)
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }

    /// Get most recent entries up to `limit`. Returns JSON string of MemoryEntry[].
    #[pyo3(signature = (limit=None, include_archived=false))]
    fn get_all(&self, limit: Option<u32>, include_archived: bool) -> PyResult<String> {
        let lim = limit.unwrap_or(200) as usize;
        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entries = store
            .get_all_with_options(lim, include_archived)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&entries)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// List entries under one path (exact + descendants). Returns MemoryEntry[] JSON.
    #[pyo3(signature = (path_prefix, limit=None, include_archived=false))]
    fn list_by_path(
        &self,
        path_prefix: &str,
        limit: Option<u32>,
        include_archived: bool,
    ) -> PyResult<String> {
        let lim = limit.unwrap_or(5000) as usize;
        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entries = store
            .list_by_path(path_prefix, lim, include_archived)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&entries)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Whether sqlite-vec extension is available for vector search.
    #[getter]
    fn vec_available(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).vec_available
    }

    // ─── Graph Operations ────────────────────────────────────────────────────

    /// Add or update an edge in the memory graph. `edge_json` is a JSON string of MemoryEdge.
    fn add_edge(&self, edge_json: &str) -> PyResult<()> {
        let edge: memory_core::MemoryEdge = serde_json::from_str(edge_json)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.inner
            .lock().unwrap_or_else(|e| e.into_inner())
            .add_edge(&edge)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Remove an edge. Returns true if found and deleted.
    #[pyo3(signature = (source_id, target_id, relation))]
    fn remove_edge(&self, source_id: &str, target_id: &str, relation: &str) -> PyResult<bool> {
        self.inner
            .lock().unwrap_or_else(|e| e.into_inner())
            .remove_edge(source_id, target_id, relation)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Get edges for a memory ID. Returns JSON string of MemoryEdge[].
    #[pyo3(signature = (memory_id, direction="both", relation_filter=None))]
    fn get_edges(
        &self,
        memory_id: &str,
        direction: &str,
        relation_filter: Option<&str>,
    ) -> PyResult<String> {
        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let edges = store
            .get_edges(memory_id, direction, relation_filter)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&edges)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// BFS graph expansion from seed IDs. Returns JSON string of GraphExpandResult.
    #[pyo3(signature = (seed_ids_json, max_hops=2, relation_filter=None))]
    fn graph_expand(
        &self,
        seed_ids_json: &str,
        max_hops: u32,
        relation_filter: Option<&str>,
    ) -> PyResult<String> {
        let seeds: Vec<String> = serde_json::from_str(seed_ids_json)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;

        let store = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let result = store
            .graph_expand(&seeds, max_hops, relation_filter)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&result)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }
}

// ── Module registration ───────────────────────────────────────────────────────

#[pymodule]
fn memory_core_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMemoryStore>()?;
    Ok(())
}
