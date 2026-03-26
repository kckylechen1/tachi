// error.rs — unified error type for memory-core

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid argument: {0}")]
    InvalidArg(String),

    #[error("Not found: {0}")]
    NotFound(String),
}
