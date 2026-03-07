// types.rs — Unified data types for memory-core
//
// This schema is the **single source of truth** for all consumers:
//   - OpenClaw memory-hybrid-bridge (Node.js via NAPI)
//   - Antigravity memory-mcp (Python via PyO3)
//   - Rust native
//
// Design: serde only, NO binding-specific macros (#[napi], #[pyclass]).
// Bindings use JSON string serialization for maximum compatibility.

use serde::{Deserialize, Serialize};

// ─── Core Entry ──────────────────────────────────────────────────────────────

/// A single memory entry, unified across all three systems.
///
/// Field mapping from legacy systems:
///   OpenClaw: entry_id → id, lossless_restatement → text, timestamp → timestamp
///   MCP:      id → id, text → text, created_at → timestamp
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// UUID primary key.
    /// Accepts "entry_id" from OpenClaw JSON for backward compat.
    #[serde(alias = "entry_id")]
    pub id: String,

    /// Hierarchical path, e.g. "/openclaw/agent-main"
    #[serde(default = "default_path")]
    pub path: String,

    /// L0 short summary (≤100 chars)
    #[serde(default)]
    pub summary: String,

    /// Full lossless text (L2). Accepts "lossless_restatement" from OpenClaw.
    #[serde(alias = "lossless_restatement")]
    pub text: String,

    /// 0.0 – 1.0 importance score
    #[serde(default = "default_importance")]
    pub importance: f64,

    /// ISO 8601 timestamp. Accepts "created_at" from MCP, "event_time" from old Rust.
    #[serde(alias = "created_at", alias = "event_time")]
    pub timestamp: String,

    /// Category: "fact" | "decision" | "experience" | "preference" | "entity" | "other"
    #[serde(default = "default_category")]
    pub category: String,

    /// Topic / subject area
    #[serde(default)]
    pub topic: String,

    /// Keyword tags (promoted from metadata for FTS indexing)
    #[serde(default)]
    pub keywords: Vec<String>,

    /// Person names mentioned
    #[serde(default)]
    pub persons: Vec<String>,

    /// Entity names mentioned (projects, tools, etc.)
    #[serde(default)]
    pub entities: Vec<String>,

    /// Physical or logical location
    #[serde(default)]
    pub location: String,

    /// How this entry was created: "manual" | "extraction" | "migration"
    #[serde(default = "default_source")]
    pub source: String,

    /// Scope: "user" | "project" | "general"
    #[serde(default = "default_scope")]
    pub scope: String,

    /// Soft-delete marker. Archived entries are excluded by default from queries.
    #[serde(default)]
    pub archived: bool,

    /// Number of times this entry has been retrieved
    #[serde(default)]
    pub access_count: i64,

    /// Last retrieval time (ISO 8601), None if never retrieved
    #[serde(default)]
    pub last_access: Option<String>,

    /// Embedding vector (1024-dim Voyage-4)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,

    /// Catch-all JSON blob for low-frequency fields:
    /// source_refs, caused_by, leads_to, etc.
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
}

// ─── Defaults ────────────────────────────────────────────────────────────────

fn default_path() -> String { "/".to_string() }
fn default_importance() -> f64 { 0.7 }
fn default_category() -> String { "fact".to_string() }
fn default_source() -> String { "manual".to_string() }
fn default_scope() -> String { "general".to_string() }
fn default_metadata() -> serde_json::Value { serde_json::Value::Object(Default::default()) }

// ─── Scoring Types ───────────────────────────────────────────────────────────

/// Per-channel scores for a single search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridScore {
    /// Cosine similarity from vector KNN (0.0–1.0)
    pub vector: f64,
    /// BM25 FTS score (normalized 0.0–1.0)
    pub fts: f64,
    /// Bag-of-words symbolic overlap (0.0–1.0)
    pub symbolic: f64,
    /// ACT-R memory decay factor
    pub decay: f64,
    /// Weighted final score
    #[serde(rename = "final")]
    pub final_score: f64,
}

/// A ranked search result: entry + scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub entry: MemoryEntry,
    pub score: HybridScore,
}

/// Aggregate statistics about the memory store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResult {
    pub total: u64,
    pub by_scope: std::collections::HashMap<String, u64>,
    pub by_category: std::collections::HashMap<String, u64>,
    pub by_root_path: std::collections::HashMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_openclaw_format() {
        let json = r#"{
            "entry_id": "m_001",
            "lossless_restatement": "Rust is fast",
            "summary": "Rust",
            "keywords": ["rust"],
            "timestamp": "2026-01-01T00:00:00Z",
            "location": "",
            "persons": ["Kyle"],
            "entities": ["OpenClaw"],
            "topic": "tech",
            "scope": "project",
            "path": "/openclaw",
            "category": "fact",
            "importance": 0.8,
            "source_refs": []
        }"#;
        let entry: MemoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "m_001");
        assert_eq!(entry.text, "Rust is fast");
        assert_eq!(entry.persons, vec!["Kyle"]);
    }

    #[test]
    fn test_deserialize_mcp_format() {
        let json = r#"{
            "id": "abc123",
            "text": "记忆系统重写",
            "created_at": "2026-02-23T12:00:00Z",
            "path": "/",
            "importance": 0.7,
            "keywords": ["记忆"]
        }"#;
        let entry: MemoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "abc123");
        assert_eq!(entry.timestamp, "2026-02-23T12:00:00Z");
        assert_eq!(entry.category, "fact"); // default
    }
}
