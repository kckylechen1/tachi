use rusqlite::Connection;

use crate::error::MemoryError;

pub fn init_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA cache_size = -16000;   -- 16 MB page cache

        CREATE TABLE IF NOT EXISTS memories (
            id           TEXT PRIMARY KEY,
            path         TEXT NOT NULL DEFAULT '/',
            summary      TEXT NOT NULL DEFAULT '',
            text         TEXT NOT NULL DEFAULT '',
            importance   REAL NOT NULL DEFAULT 0.7,
            timestamp    TEXT NOT NULL,
            category     TEXT NOT NULL DEFAULT 'fact',
            topic        TEXT NOT NULL DEFAULT '',
            keywords     TEXT NOT NULL DEFAULT '[]',
            persons      TEXT NOT NULL DEFAULT '[]',
            entities     TEXT NOT NULL DEFAULT '[]',
            location     TEXT NOT NULL DEFAULT '',
            source       TEXT NOT NULL DEFAULT 'manual',
            scope        TEXT NOT NULL DEFAULT 'general',
            archived     INTEGER NOT NULL DEFAULT 0,
            created_at   TEXT NOT NULL DEFAULT '',
            updated_at   TEXT NOT NULL DEFAULT '',
            access_count INTEGER NOT NULL DEFAULT 0,
            last_access  TEXT,
            revision     INTEGER NOT NULL DEFAULT 1,
            metadata     TEXT NOT NULL DEFAULT '{}'
        );

        CREATE INDEX IF NOT EXISTS idx_memories_path        ON memories(path);
        CREATE INDEX IF NOT EXISTS idx_memories_importance  ON memories(importance DESC);
        CREATE INDEX IF NOT EXISTS idx_memories_timestamp   ON memories(timestamp DESC);

        -- Standalone FTS5 table with Chinese + Pinyin tokenizer.
        -- Uses wangfenjin/simple for CJK segmentation.
        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
            id UNINDEXED,
            path,
            summary,
            text,
            keywords,
            entities,
            tokenize = 'simple'
        );

        -- Memory graph edges for causal/temporal/entity relationships
        CREATE TABLE IF NOT EXISTS memory_edges (
            source_id  TEXT NOT NULL,
            target_id  TEXT NOT NULL,
            relation   TEXT NOT NULL,
            weight     REAL NOT NULL DEFAULT 1.0,
            metadata   TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (source_id, target_id, relation)
        );
        CREATE INDEX IF NOT EXISTS idx_edges_source ON memory_edges(source_id);
        CREATE INDEX IF NOT EXISTS idx_edges_target ON memory_edges(target_id);
        CREATE INDEX IF NOT EXISTS idx_edges_relation ON memory_edges(relation);

        -- Deterministic KV state (no vector search, no LLM)
        CREATE TABLE IF NOT EXISTS hard_state (
            namespace        TEXT NOT NULL,
            key              TEXT NOT NULL,
            value_json       TEXT NOT NULL DEFAULT '{}',
            version          INTEGER NOT NULL DEFAULT 1,
            created_at       TEXT NOT NULL DEFAULT '',
            updated_at       TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (namespace, key)
        );

        -- Access history for ACT-R base-level activation
        CREATE TABLE IF NOT EXISTS access_history (
            memory_id  TEXT NOT NULL,
            accessed_at TEXT NOT NULL,
            query_hash  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_access_hist_mem ON access_history(memory_id);
        CREATE INDEX IF NOT EXISTS idx_access_hist_time ON access_history(accessed_at DESC);
        CREATE INDEX IF NOT EXISTS idx_access_hist_mem_time ON access_history(memory_id, accessed_at DESC);

        -- Derived items (causal extractions, distilled rules, etc.)
        CREATE TABLE IF NOT EXISTS derived_items (
            id         TEXT PRIMARY KEY,
            text       TEXT NOT NULL DEFAULT '',
            path       TEXT NOT NULL DEFAULT '/',
            summary    TEXT NOT NULL DEFAULT '',
            importance REAL NOT NULL DEFAULT 0.5,
            source     TEXT NOT NULL DEFAULT '',
            scope      TEXT NOT NULL DEFAULT 'general',
            metadata   TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS processed_events (
            event_hash TEXT NOT NULL,
            event_id   TEXT NOT NULL DEFAULT '',
            worker     TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (event_hash, worker)
        );
        CREATE INDEX IF NOT EXISTS idx_processed_events_created_at ON processed_events(created_at DESC);

        -- Hub capability registry (skills, plugins, MCP servers)
        CREATE TABLE IF NOT EXISTS hub_capabilities (
            id          TEXT PRIMARY KEY,
            type        TEXT NOT NULL,
            name        TEXT NOT NULL,
            version     INTEGER NOT NULL DEFAULT 1,
            description TEXT NOT NULL DEFAULT '',
            definition  TEXT NOT NULL DEFAULT '',
            enabled     INTEGER NOT NULL DEFAULT 1,
            review_status TEXT NOT NULL DEFAULT 'approved',
            health_status TEXT NOT NULL DEFAULT 'healthy',
            last_error    TEXT,
            last_success_at TEXT,
            last_failure_at TEXT,
            fail_streak   INTEGER NOT NULL DEFAULT 0,
            active_version TEXT,
            exposure_mode TEXT NOT NULL DEFAULT 'direct',
            uses        INTEGER NOT NULL DEFAULT 0,
            successes   INTEGER NOT NULL DEFAULT 0,
            failures    INTEGER NOT NULL DEFAULT 0,
            avg_rating  REAL NOT NULL DEFAULT 0.0,
            last_used   TEXT,
            created_at  TEXT NOT NULL DEFAULT '',
            updated_at  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_hub_cap_type ON hub_capabilities(type);
        CREATE INDEX IF NOT EXISTS idx_hub_cap_name ON hub_capabilities(name);
        CREATE INDEX IF NOT EXISTS idx_hub_cap_enabled ON hub_capabilities(enabled);
        CREATE INDEX IF NOT EXISTS idx_hub_cap_review_status ON hub_capabilities(review_status);
        CREATE INDEX IF NOT EXISTS idx_hub_cap_health_status ON hub_capabilities(health_status);

        CREATE TABLE IF NOT EXISTS hub_version_routes (
            alias_id TEXT PRIMARY KEY,
            active_capability_id TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_hub_route_target ON hub_version_routes(active_capability_id);

        CREATE TABLE IF NOT EXISTS virtual_capability_bindings (
            vc_id         TEXT NOT NULL,
            capability_id TEXT NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 100,
            version_pin   INTEGER,
            enabled       INTEGER NOT NULL DEFAULT 1,
            metadata      TEXT NOT NULL DEFAULT '{}',
            created_at    TEXT NOT NULL DEFAULT '',
            updated_at    TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (vc_id, capability_id)
        );
        CREATE INDEX IF NOT EXISTS idx_vc_binding_capability
            ON virtual_capability_bindings(capability_id);
        CREATE INDEX IF NOT EXISTS idx_vc_binding_priority
            ON virtual_capability_bindings(vc_id, priority ASC, capability_id ASC);

        -- Audit log for proxy tool calls
        CREATE TABLE IF NOT EXISTS audit_log (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp   TEXT NOT NULL,
            server_id   TEXT NOT NULL,
            tool_name   TEXT NOT NULL,
            args_hash   TEXT NOT NULL DEFAULT '',
            success     INTEGER NOT NULL DEFAULT 1,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            error_kind  TEXT,
            created_at  TEXT NOT NULL DEFAULT (STRFTIME('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp DESC);
        CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_log(server_id);
        CREATE INDEX IF NOT EXISTS idx_audit_created_at ON audit_log(created_at DESC);

        -- Agent known state for context diffing (incremental memory sync)
        CREATE TABLE IF NOT EXISTS agent_known_state (
            agent_id   TEXT NOT NULL,
            memory_id  TEXT NOT NULL,
            revision   INTEGER NOT NULL DEFAULT 0,
            synced_at  TEXT NOT NULL,
            PRIMARY KEY (agent_id, memory_id)
        );
        CREATE INDEX IF NOT EXISTS idx_agent_known_agent ON agent_known_state(agent_id);
        CREATE INDEX IF NOT EXISTS idx_agent_known_memory ON agent_known_state(memory_id);
        CREATE INDEX IF NOT EXISTS idx_agent_known_synced_at ON agent_known_state(synced_at DESC);

        -- Sandbox rules for role-based memory isolation (Semantic Sandboxing)
        CREATE TABLE IF NOT EXISTS sandbox_rules (
            agent_role   TEXT NOT NULL,
            path_pattern TEXT NOT NULL,
            access_level TEXT NOT NULL DEFAULT 'read',
            created_at   TEXT NOT NULL,
            PRIMARY KEY (agent_role, path_pattern)
        );
        CREATE INDEX IF NOT EXISTS idx_sandbox_role ON sandbox_rules(agent_role);

        -- Sandbox runtime policies for MCP capability execution control
        CREATE TABLE IF NOT EXISTS sandbox_policies (
            capability_id    TEXT PRIMARY KEY,
            runtime_type     TEXT NOT NULL DEFAULT 'process',
            env_allowlist    TEXT NOT NULL DEFAULT '[]',
            fs_read_roots    TEXT NOT NULL DEFAULT '[]',
            fs_write_roots   TEXT NOT NULL DEFAULT '[]',
            cwd_roots        TEXT NOT NULL DEFAULT '[]',
            max_startup_ms   INTEGER NOT NULL DEFAULT 30000,
            max_tool_ms      INTEGER NOT NULL DEFAULT 30000,
            max_concurrency  INTEGER NOT NULL DEFAULT 1,
            enabled          INTEGER NOT NULL DEFAULT 1,
            created_at       TEXT NOT NULL DEFAULT '',
            updated_at       TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_sandbox_policy_enabled ON sandbox_policies(enabled);

        -- Sandbox execution audit for preflight/runtime decisions.
        CREATE TABLE IF NOT EXISTS sandbox_exec_audit (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp      TEXT NOT NULL,
            capability_id  TEXT NOT NULL,
            stage          TEXT NOT NULL DEFAULT 'preflight',
            decision       TEXT NOT NULL,
            reason         TEXT,
            duration_ms    INTEGER NOT NULL DEFAULT 0,
            tool_name      TEXT,
            error_kind     TEXT,
            metadata       TEXT NOT NULL DEFAULT '{}',
            created_at     TEXT NOT NULL DEFAULT (STRFTIME('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );
        CREATE INDEX IF NOT EXISTS idx_sandbox_exec_timestamp ON sandbox_exec_audit(timestamp DESC);
        CREATE INDEX IF NOT EXISTS idx_sandbox_exec_capability ON sandbox_exec_audit(capability_id);

        -- Ghost persistence: messages, subscriptions, cursors, topics, reflections.
        CREATE TABLE IF NOT EXISTS ghost_messages (
            id            TEXT PRIMARY KEY,
            topic         TEXT NOT NULL,
            topic_index   INTEGER NOT NULL,
            payload       TEXT NOT NULL DEFAULT '{}',
            publisher     TEXT NOT NULL DEFAULT '',
            timestamp     TEXT NOT NULL DEFAULT '',
            promoted      INTEGER NOT NULL DEFAULT 0,
            importance    REAL NOT NULL DEFAULT 0.5,
            created_at    TEXT NOT NULL DEFAULT ''
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_ghost_topic_index_unique
            ON ghost_messages(topic, topic_index);
        CREATE INDEX IF NOT EXISTS idx_ghost_topic_index
            ON ghost_messages(topic, topic_index DESC);
        CREATE INDEX IF NOT EXISTS idx_ghost_messages_timestamp
            ON ghost_messages(timestamp DESC);

        CREATE TABLE IF NOT EXISTS ghost_subscriptions (
            agent_id      TEXT NOT NULL,
            topic         TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT '',
            updated_at    TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (agent_id, topic)
        );
        CREATE INDEX IF NOT EXISTS idx_ghost_subscriptions_topic
            ON ghost_subscriptions(topic);

        CREATE TABLE IF NOT EXISTS ghost_cursors (
            agent_id        TEXT NOT NULL,
            topic           TEXT NOT NULL,
            last_seen_index INTEGER NOT NULL DEFAULT 0,
            updated_at      TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (agent_id, topic)
        );
        CREATE INDEX IF NOT EXISTS idx_ghost_cursors_updated
            ON ghost_cursors(updated_at DESC);

        CREATE TABLE IF NOT EXISTS ghost_topics (
            topic             TEXT PRIMARY KEY,
            total_published   INTEGER NOT NULL DEFAULT 0,
            last_message_time TEXT,
            last_publisher    TEXT,
            updated_at        TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_ghost_topics_last_message
            ON ghost_topics(last_message_time DESC);

        CREATE TABLE IF NOT EXISTS ghost_reflections (
            id          TEXT PRIMARY KEY,
            agent_id    TEXT NOT NULL,
            topic       TEXT,
            summary     TEXT NOT NULL DEFAULT '',
            metadata    TEXT NOT NULL DEFAULT '{}',
            created_at  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_ghost_reflections_topic
            ON ghost_reflections(topic);
        CREATE INDEX IF NOT EXISTS idx_ghost_reflections_created
            ON ghost_reflections(created_at DESC);
    "#)?;

    // Forward-compatible migrations for existing DB files created before
    // archived/created_at/updated_at columns existed.
    ensure_column(conn, "memories", "archived", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "memories", "created_at", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "memories", "updated_at", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "memories", "revision", "INTEGER NOT NULL DEFAULT 1")?;

    // Temporal edge columns for memory_edges
    ensure_column(
        conn,
        "memory_edges",
        "valid_from",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(conn, "memory_edges", "valid_to", "TEXT")?;

    // derived_items columns that may be missing on legacy databases
    ensure_column(conn, "derived_items", "summary", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(
        conn,
        "derived_items",
        "importance",
        "REAL NOT NULL DEFAULT 0.5",
    )?;
    ensure_column(
        conn,
        "derived_items",
        "scope",
        "TEXT NOT NULL DEFAULT 'general'",
    )?;
    ensure_column(
        conn,
        "derived_items",
        "created_at",
        "TEXT NOT NULL DEFAULT ''",
    )?;

    // Hub governance columns for review + health + routing metadata
    ensure_column(
        conn,
        "hub_capabilities",
        "review_status",
        "TEXT NOT NULL DEFAULT 'approved'",
    )?;
    ensure_column(
        conn,
        "hub_capabilities",
        "health_status",
        "TEXT NOT NULL DEFAULT 'healthy'",
    )?;
    ensure_column(conn, "hub_capabilities", "last_error", "TEXT")?;
    ensure_column(conn, "hub_capabilities", "last_success_at", "TEXT")?;
    ensure_column(conn, "hub_capabilities", "last_failure_at", "TEXT")?;
    ensure_column(
        conn,
        "hub_capabilities",
        "fail_streak",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "hub_capabilities", "active_version", "TEXT")?;
    ensure_column(
        conn,
        "hub_capabilities",
        "exposure_mode",
        "TEXT NOT NULL DEFAULT 'direct'",
    )?;

    // Indexes on migrated columns — MUST come after ensure_column so the
    // columns exist on legacy databases that were created without them.
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_memories_archived    ON memories(archived);
        CREATE INDEX IF NOT EXISTS idx_memories_last_access ON memories(last_access DESC);
        CREATE INDEX IF NOT EXISTS idx_derived_source       ON derived_items(source);
        CREATE INDEX IF NOT EXISTS idx_derived_path         ON derived_items(path);
        CREATE INDEX IF NOT EXISTS idx_derived_created_at   ON derived_items(created_at DESC);
    "#,
    )?;

    // Backfill empty values for legacy rows.
    conn.execute(
        "UPDATE memories SET created_at = timestamp WHERE created_at IS NULL OR created_at = ''",
        [],
    )?;
    conn.execute(
        "UPDATE memories SET updated_at = created_at WHERE updated_at IS NULL OR updated_at = ''",
        [],
    )?;
    conn.execute(
        "UPDATE memories SET revision = 1 WHERE revision IS NULL OR revision <= 0",
        [],
    )?;

    ensure_fts_backfilled(conn)?;

    // NOTE: sqlite-vec virtual table (memories_vec) is created separately after
    // the extension is loaded by the caller via register_sqlite_vec().
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), MemoryError> {
    if has_column(conn, table, column)? {
        return Ok(());
    }

    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
    conn.execute(&sql, [])?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, MemoryError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_fts_backfilled(conn: &Connection) -> Result<(), MemoryError> {
    let memories_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    if memories_count == 0 {
        return Ok(());
    }

    let fts_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM memories_fts", [], |row| row.get(0))?;
    if fts_count > 0 {
        return Ok(());
    }

    conn.execute(
        r#"INSERT INTO memories_fts (id, path, summary, text, keywords, entities)
           SELECT
             id,
             path,
             summary,
             text,
             trim(replace(replace(replace(keywords, '[', ' '), ']', ' '), '"', ' ')),
             trim(replace(replace(replace(entities, '[', ' '), ']', ' '), '"', ' '))
           FROM memories"#,
        [],
    )?;

    Ok(())
}
