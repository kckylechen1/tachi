# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.2] - 2026-03-23

### Added
- **Sigil Hub Phase 1 — Capability Registry + Discovery**: A unified catalog for Skills, Plugins, and MCP Servers. Any agent connecting to Sigil can discover and retrieve all registered capabilities.
- **`hub_capabilities` table** in `memory-core`: New SQLite table with CRUD operations for registering, listing, searching, enabling/disabling, and tracking usage metrics of capabilities.
- **`hub.rs` types module**: `HubCapability` struct with id, type, name, version, description, definition, enabled, usage/success/failure counters, and rolling average rating.
- **5 new MCP tools**: `hub_register` (register a capability), `hub_discover` (list/search with dual-DB merge, project shadows global), `hub_get` (fetch single capability with project-first fallback), `hub_feedback` (record success/failure/rating), `hub_stats` (aggregated metrics across both DBs).
- **4 new NAPI methods** for OpenClaw: `hub_register`, `hub_discover`, `hub_get`, `hub_feedback` — same Hub functionality available to Node.js agents.
- **Skill batch loader** (`scripts/load_skills_to_hub.py`): Scans `~/.claude/skills/` directories, parses YAML frontmatter + full markdown content, and bulk-registers into global Hub. Successfully loaded 67 skills.
- **Hub dual-DB inheritance**: Project Hub capabilities shadow global ones by ID. Register defaults to project DB, discover/get queries both with project priority.

### Changed
- **`memory-core` public API**: Added `pub mod hub` and re-exported `HubCapability`. Added 7 Hub methods to `MemoryStore` (hub_register, hub_get, hub_list, hub_search, hub_set_enabled, hub_record_feedback, hub_delete).
- **MCP tool list**: Server now exposes 15 tools (10 memory + 5 hub).

## [0.5.0] - 2026-03-23

### Added
- **Dual-DB Architecture (Global + Project)**: Memory server now maintains two separate SQLite databases — a global DB (`~/.sigil/global/memory.db`) for cross-project knowledge (user preferences, universal facts) and a per-project DB (`.sigil/memory.db` at git root) for project-scoped memories (architecture decisions, codebase patterns). This is the foundation for multi-agent shared memory.
- **`DbScope` enum**: All MCP tool responses now include a `"db"` field (`"global"` or `"project"`) indicating which database sourced or stored the result.
- **Automatic Git root detection**: Server detects the nearest `.git` directory to resolve the project DB path. Falls back to global-only when not inside a git repository.
- **Legacy DB migration**: On first run, automatically migrates `~/.sigil/memory.db` to `~/.sigil/global/memory.db` for seamless upgrade from v0.4.
- **Dual-DB search merge**: `search_memory` queries both databases in parallel, merges results by `final_score` descending, deduplicates by entry ID, and truncates to `top_k`.
- **Write scope routing**: `save_memory`, `extract_facts`, and `ingest_event` route writes via `resolve_write_scope()` — `"global"` scope writes to global DB, everything else defaults to project DB (with automatic fallback + warning when no project DB is available).
- **Per-DB integrity checks**: Startup runs `PRAGMA quick_check` on both databases independently.
- **Aggregated `memory_stats`**: Returns merged totals across both DBs plus a `"databases"` breakdown showing per-DB stats and vector availability.

### Changed
- **`MemoryServer` struct**: Split from single `db_path`/`vec_available` into `global_db_path`/`project_db_path` and `global_vec_available`/`project_vec_available`.
- **`set_state`/`get_state`**: Now hardcoded to use global DB (server state is cross-project by nature).
- **`default_scope()`**: Changed from `"general"` to `"project"` to match new dual-DB routing semantics.
- **`get_memory`**: Tries project DB first, falls back to global DB.
- **`list_memories`**: Queries both DBs, tags entries with source, sorts by timestamp descending.

### Removed
- **Single-DB `with_store` helper**: Replaced by `with_global_store`, `with_project_store`, and `with_store_for_scope`.
- **Pipeline module**: Removed in prior commit (server is now pure MCP handler).

## [0.4.0] - 2026-03-18

### Added
- **NEW: Native Rust MCP Server (`memory-server` crate)**: Complete replacement for the Python `mcp/server.py`. Single 5.2MB ARM64 binary with 10 MCP tools, built with `rmcp` SDK. Eliminates Python runtime dependency for the MCP server.
- **LLM Integration in Rust (`llm.rs`)**: Voyage-4 embedding via `reqwest` (direct API) and SiliconFlow Qwen LLM via `async-openai` for L0 summary generation and fact extraction. All API calls happen asynchronously before database locks.
- **Prompt Templates (`prompts.rs`)**: Extracted and hardcoded all LLM prompt templates (EXTRACTION_PROMPT, SUMMARY_PROMPT, CAUSAL_PROMPT) from Python into Rust constants.
- **10 MCP Tools**: `save_memory` (with real-time Voyage-4 embedding + Qwen summary), `search_memory`, `get_memory`, `list_memories`, `memory_stats`, `set_state`, `get_state`, `extract_facts` (LLM-based), `ingest_event`, `get_pipeline_status`.
- **Hard State Table in Rust**: `hard_state` table with `set_state`/`get_state` functions in `memory-core` for persistent KV storage.
- **Memory Graph (`memory_edges` table)**: Added graph structure with edge management, PageRank scoring, and graph expansion in hybrid search.
- **ACT-R Cognitive Decay**: Time-based decay scoring inspired by ACT-R cognitive architecture, integrated into hybrid search scorer.
- **PageRank Integration**: Graph-aware PageRank scoring in hybrid search for importance-weighted retrieval.
- **Noise Injection**: Configurable Gaussian noise for search score diversification.

### Changed
- **Architecture**: MCP server can now run as either Python (`mcp/server.py`) or native Rust binary (`memory-server`). Rust path eliminates PyO3 bridge overhead.
- **Embedding Decision**: A/B tested Voyage-4 vs Qwen3-Embedding-8B (SiliconFlow). Voyage-4 won on discrimination (Δ 0.46 vs 0.37) and query latency (569ms vs 1017ms). Staying with Voyage-4.
- **Thread Safety**: Replaced `tokio::sync::Mutex` with `std::sync::Mutex` for `rusqlite::Connection` (!Send safety in multi-threaded Tokio runtime).
- **JSON Safety**: All JSON output uses `serde_json::to_string` instead of `format!` string concatenation.

### Fixed
- **ServerHandler Registration**: Fixed `rmcp` integration where `#[tool(tool_box)]` was missing on `ServerHandler` impl, causing tools to not register (empty `tools/list` response).
- **Search Output Format**: `search_memory` returns human-readable summaries instead of raw JSON.
- **State Management**: Proper `hard_state` table replaces hacky MemoryEntry-based state storage.

## [0.3.0] - 2026-03-14

### Added
- **Core/MCP**: Introduced `hard_state` table and corresponding `set_state` / `get_state` endpoints to store rigid KV data distinct from semantic mappings. This resolves data hallucination for strict key values like dynamic watchlists.
- **MCP/Workers**: Created `derived_items` table explicitly isolating Causal logic and Distillation outputs from primary empirical facts, drastically improving search relevancy.
- **Server**: Implemented `ENABLE_PIPELINE` logic (defaulting to false) to toggle the background asynchronous extraction process on demand to maximize pure querying speeds.

### Changed
- **Pipeline**: Shifted event extraction process to lazy-execution triggered under the background thread pool queue. 
- **Core/MCP**: Removed all LLM-based abstract summarization fallback functions from standard `save_memory` path to bypass latency delays.

### Removed
- **MCP**: Eliminated `Voyage-Rerank-2.5` dependency from standard hybrid searches. Core Rust pipeline handles similarity filtering accurately enough, boosting response time natively via KNN & FTS5 mechanisms alone.
- **Scrap**: Cleaned up legacy unmaintained prototype directories `memory-mcp/` and `memory-core-rs/` from local `scratch` areas.

## [0.2.1] - 2026-03-13

### Fixed
- **MCP/Extractor**: Fixed `httpx.ReadTimeout` empty error bug in `extract_facts` and added 3-round exponential backoff (2s/4s/8s) for Siliconflow API calls to improve retry resilience against transient network failures.
- **Config**: Fixed `config.ts` environmental variable parsing where `MEMORY_DB_PATH` was not expanding `~` to home directory. Additionally ensured `install_openclaw_ext.sh` creates the necessary `data` directory.
- **Memory Deduplication** (PR #4): Merged two-stage memory deduplication (`HARD_SKIP` vs `EVOLVE`) with upsert indentation bug fix. This resolves the issue of over-aggressive memory deduplication. Tests confirmed this mathematical threshold approach is more robust than LLM-based (GLM-4/Qwen/DeepSeek) deduplication judgments for this specific pipeline.

## [0.2.0] - 2026-03-08

### Added
- **MCP/Workers**: Activated causal worker pipeline for asynchronous extraction of cause-and-effect relationships.
- **Core**: Refactored `memory_relations` support to allow robust linking of related memory fragments.
- **Docs**: Added one-click install script for OpenClaw.
- **Docs**: Added Sigil v2 PRD (architecture specification) for causal pipelines and memory workers.
- **Async Pipeline**: Phase 2 async event pipeline with 4 memory workers (Extractor, Distiller, CausalWorker, Consolidator).

### Changed
- **MCP/Extractor**: Upgraded default fact extraction model to `Qwen3.5-27B` for significantly better causal relationship tracking and structured fact parsing.
- **Docs**: Updated the recommended extraction model in `README.zh-CN.md` and `README.md` to `Qwen3.5-27B`.

### Fixed
- **Core**: Stabilized vector KNN searches and ensured `auto-capture` remains writable.
- **Core**: Addressed SQLite Upsert limitations by migrating to `DELETE` + `INSERT` for `sqlite-vec` `vec0` virtual tables.
- **OpenClaw Plugin**: Fixed critical hooks and synced all improvements to the latest agent platform requirements.
- **OpenClaw Plugin**: Corrected plugin kind to `memory`, removed dead code paths, and added native Voyage reranker support.
- **OpenClaw Plugin**: Ensure `installer` reliably builds bindings and loads the plugin successfully.
- **Python MCP**: Resolved code smells during fact extraction and migrations.

## [0.1.0] - 2026-03-05

### Added
- Initial release of the Sigil Memory System.
- Blazing Fast Rust Core (`memory-core`) featuring Native CJK FTS5 text search and `sqlite-vec` semantic indexing.
- 4-Channel Hybrid Search Engine (Semantic, Lexical, Symbolic, Decay).
- Native Node.js `NAPI-RS` bindings for OpenClaw extension.
- Native Python `PyO3` bindings targeting MCP server frameworks.
- Added Dotenv support for graceful API key extraction from project roots.
- Support for Voyage-4, Voyage Rerank-2.5, and GLM-4 base models.
