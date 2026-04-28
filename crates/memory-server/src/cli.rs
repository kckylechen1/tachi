use clap::{Parser, Subcommand};
use std::path::PathBuf;

// ─── CLI Arguments ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "tachi", version, about = "Tachi — memory + Hub MCP server")]
pub(crate) struct Cli {
    /// Run as HTTP daemon instead of stdio transport
    #[arg(long)]
    pub daemon: bool,

    /// Port for HTTP daemon (default: 6919)
    #[arg(long, default_value_t = 6919)]
    pub port: u16,

    /// Override global memory DB path (equivalent to MEMORY_DB_PATH)
    #[arg(long, value_name = "PATH")]
    pub global_db: Option<PathBuf>,

    /// Override project memory DB path
    #[arg(long, value_name = "PATH")]
    pub project_db: Option<PathBuf>,

    /// Disable project DB entirely (force single-DB mode)
    #[arg(long)]
    pub no_project_db: bool,

    /// Built-in tool surface bundles or host alias, e.g. remember, observe+coordinate, openclaw, admin
    #[arg(long)]
    pub profile: Option<String>,

    /// Enable/disable background database GC (overrides MEMORY_GC_ENABLED)
    #[arg(long)]
    pub gc_enabled: Option<bool>,

    /// Delay before first background GC run in seconds (overrides MEMORY_GC_INITIAL_DELAY_SECS)
    #[arg(long)]
    pub gc_initial_delay_secs: Option<u64>,

    /// Interval between background GC runs in seconds (overrides MEMORY_GC_INTERVAL_SECS)
    #[arg(long)]
    pub gc_interval_secs: Option<u64>,

    /// Enable clawdoctor: periodic OpenClaw health check + auto-restart
    /// (overrides CLAWDOCTOR_ENABLED)
    #[arg(long)]
    pub clawdoctor: Option<bool>,

    /// OpenClaw gateway URL for clawdoctor health checks
    /// (overrides CLAWDOCTOR_URL, default: http://127.0.0.1:18789)
    #[arg(long)]
    pub clawdoctor_url: Option<String>,

    /// Clawdoctor check interval in seconds (overrides CLAWDOCTOR_INTERVAL_SECS, default: 300)
    #[arg(long)]
    pub clawdoctor_interval_secs: Option<u64>,

    /// Consecutive failures before clawdoctor triggers a restart
    /// (overrides CLAWDOCTOR_FAIL_THRESHOLD, default: 3)
    #[arg(long)]
    pub clawdoctor_fail_threshold: Option<u32>,

    /// CLI command (defaults to `serve` when omitted)
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Commands {
    /// Start MCP Server (default when no subcommand is provided)
    Serve,
    /// Search memories
    Search {
        query: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value_t = 5)]
        top_k: usize,
    },
    /// Save a memory
    Save {
        text: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        importance: Option<f64>,
    },
    /// Show database statistics
    Stats,
    /// Inspect onboarding readiness and current local Tachi setup
    Setup {
        /// Emit machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
    },
    /// Scan for fragmented memory databases and report consolidation candidates
    Tidy {
        /// Emit machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
        /// Execute the conservative apply path for clearly safe actions
        #[arg(long)]
        apply: bool,
    },
    /// Doctor v2 — extension-aware DB classification + safe auto-fix
    Doctor {
        /// Emit machine-readable JSON instead of the human summary
        #[arg(long)]
        json: bool,
        /// Skip the safe auto-fix pass (placeholder quarantine + WAL copy-aside)
        #[arg(long)]
        scan_only: bool,
        /// Override default scan roots (~/.tachi, ~/.openclaw, ~/.sigil, ~/.gemini/antigravity)
        #[arg(long, value_name = "PATH")]
        roots: Vec<PathBuf>,
        /// Branch #5: also report a foundry job-status histogram per manifest DB
        #[arg(long)]
        jobs: bool,
    },
    /// Manifest v1 — show/init/refresh ~/.tachi/manifest.json
    Manifest {
        #[command(subcommand)]
        action: ManifestAction,
    },
    /// Run garbage collection
    Gc,
    /// Hub management
    Hub {
        #[command(subcommand)]
        action: HubAction,
    },
    /// Backfill missing vector embeddings using Voyage API
    BackfillVectors {
        /// Target DB path (defaults to global DB)
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
        /// Batch size for Voyage API calls (max 128)
        #[arg(long, default_value_t = 64)]
        batch_size: usize,
        /// Only count missing entries, don't embed
        #[arg(long)]
        dry_run: bool,
    },
    /// Backfill missing summaries using the configured summary LLM
    BackfillSummaries {
        /// Target DB path (defaults to global DB)
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
        /// Only count missing entries, don't generate summaries
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild or backfill FTS5 full-text search index
    BackfillFts {
        /// Target DB path (defaults to global DB)
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
        /// Drop and fully rebuild the FTS table (useful after corruption)
        #[arg(long)]
        full: bool,
        /// Only show stats, don't modify
        #[arg(long)]
        dry_run: bool,
    },
    /// Branch #6 — Rescue: split a multi-project memory.db into per-project Tachi DBs.
    /// Plan-only by default; pass --apply to actually write into target DBs.
    Rescue {
        #[command(subcommand)]
        action: RescueAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum RescueAction {
    /// Split the legacy antigravity memory.db into per-project Tachi DBs.
    Antigravity {
        /// Source DB path (defaults to ~/.gemini/antigravity/memory.db)
        #[arg(long, value_name = "PATH")]
        source: Option<PathBuf>,
        /// Root directory containing per-project Tachi DBs (defaults to ~/.tachi/projects)
        #[arg(long, value_name = "PATH")]
        targets_root: Option<PathBuf>,
        /// Actually perform the rescue (default: dry-run plan only).
        #[arg(long)]
        apply: bool,
        /// Emit machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ManifestAction {
    /// Show the current manifest (human or --json)
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Create a new manifest by running a doctor scan (idempotent)
    Init,
    /// Re-run doctor scan and update the manifest in place
    Refresh,
    /// Resolve a path or scope hint against the manifest (diagnostic)
    Resolve {
        /// Path or scope hint (e.g. "global", "project:hyperion", "/abs/path/memory.db")
        target: String,
    },
    /// Plan or apply a sweep of unowned placeholder/backup DB files (dry-run by default)
    Sweep {
        /// Actually move files to quarantine (default: dry-run)
        #[arg(long)]
        apply: bool,
        /// Output JSON instead of human text
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum HubAction {
    List {
        #[arg(long)]
        cap_type: Option<String>,
    },
    Register {
        id: String,
        #[arg(long)]
        cap_type: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        definition: String,
        #[arg(long)]
        description: Option<String>,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Stats,
}
