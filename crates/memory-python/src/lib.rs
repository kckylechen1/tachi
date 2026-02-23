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
use pyo3::types::PyDict;
use memory_core::{MemoryStore as RustStore, SearchOptions, MemoryEntry};
use serde_json::Value;

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
        self.inner
            .lock()
            .unwrap()
            .upsert(&me)
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

        let results = self
            .inner
            .lock()
            .unwrap()
            .search(query, Some(opts))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&results)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Fetch a single memory by ID. Returns JSON string or None.
    #[pyo3(signature = (id))]
    fn get(&self, id: &str) -> PyResult<Option<String>> {
        let entry = self
            .inner
            .lock()
            .unwrap()
            .get(id)
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
    #[pyo3(signature = (limit=None))]
    fn get_all(&self, limit: Option<u32>) -> PyResult<String> {
        let lim = limit.unwrap_or(200) as usize;
        let entries = self
            .inner
            .lock()
            .unwrap()
            .get_all(lim)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        serde_json::to_string(&entries)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    /// Whether sqlite-vec extension is available for vector search.
    #[getter]
    fn vec_available(&self) -> bool {
        self.inner.lock().unwrap().vec_available
    }
}

// ── Module registration ───────────────────────────────────────────────────────

#[pymodule]
fn memory_core_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMemoryStore>()?;
    Ok(())
}
