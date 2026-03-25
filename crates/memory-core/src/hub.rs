// hub.rs — Hub capability types for Sigil Hub registry
//
// Defines the data model for Skills, Plugins, and MCP server configs
// that are registered, discovered, and tracked through the Hub.

use serde::{Deserialize, Serialize};

/// A registered capability in the Sigil Hub.
///
/// Capabilities come in three types:
/// - **Skill**: A prompt template + orchestration logic
/// - **Plugin**: A code module (WASM, dylib, or JS)
/// - **MCP Server**: An external tool server connection config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCapability {
    /// Unique identifier, e.g. "skill:code-review", "mcp:github"
    pub id: String,

    /// Type: "skill" | "plugin" | "mcp"
    pub cap_type: String,

    /// Human-readable name
    pub name: String,

    /// Version number (monotonically increasing)
    pub version: u32,

    /// Short description of what this capability does
    pub description: String,

    /// JSON string containing the capability definition:
    /// - Skill: prompt template, tools_required, trigger patterns
    /// - Plugin: module path, entry point, dependencies
    /// - MCP: server command, args, env, transport config
    pub definition: String,

    /// Whether this capability is currently active
    pub enabled: bool,

    /// Total number of invocations
    pub uses: u64,

    /// Number of successful invocations
    pub successes: u64,

    /// Number of failed invocations
    pub failures: u64,

    /// Running average user rating (0.0 - 5.0)
    pub avg_rating: f64,

    /// ISO 8601 timestamp of last use
    pub last_used: Option<String>,

    /// ISO 8601 timestamp of creation
    pub created_at: String,

    /// ISO 8601 timestamp of last update
    pub updated_at: String,
}
