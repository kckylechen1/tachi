// lib.rs — Public API for memory-core
//
// Re-exports all primary types and provides a MemoryStore handle that
// bundles a rusqlite::Connection with convenience methods.

pub mod db;
pub mod error;
pub mod scorer;
pub mod search;
pub mod types;

pub use error::MemoryError;
pub use types::{HybridScore, MemoryEntry, SearchResult};
pub use search::{SearchOptions, hybrid_search};
pub use scorer::HybridWeights;

use rusqlite::Connection;

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
        db::init_schema(&conn)?;
        let vec_available = db::try_load_sqlite_vec(&conn);
        Ok(Self { conn, vec_available })
    }

    /// Insert or update a memory entry (with optional embedding vector).
    pub fn upsert(&self, entry: &MemoryEntry) -> Result<(), MemoryError> {
        db::upsert(&self.conn, entry, self.vec_available)
    }

    /// Hybrid search: Text + FTS5 + optional vector channel.
    pub fn search(
        &self,
        query: &str,
        opts: Option<SearchOptions>,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let mut options = opts.unwrap_or_default();
        options.vec_available = self.vec_available;
        hybrid_search(&self.conn, query, &options)
    }

    /// Fetch a single entry by ID.
    pub fn get(&self, id: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        let ids = vec![id.to_string()];
        let mut map = db::fetch_by_ids(&self.conn, &ids)?;
        Ok(map.remove(id))
    }

    /// Low-level access for bindings that need raw Connection reference.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Fetch multiple newest entries up to a limit (used for dedup).
    pub fn get_all(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        db::get_all(&self.conn, limit)
    }
}
