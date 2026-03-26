use super::*;

fn open_cli_store(db_path: &PathBuf) -> Result<MemoryStore, Box<dyn std::error::Error>> {
    let db_str = db_path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("DB path contains invalid UTF-8: {}", db_path.display()),
        )
    })?;
    Ok(MemoryStore::open(db_str)?)
}

fn print_pretty_json(value: &serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn build_cli_memory_entry(
    id: String,
    text: String,
    path: Option<String>,
    importance: Option<f64>,
    timestamp: String,
) -> MemoryEntry {
    MemoryEntry {
        id,
        path: path.unwrap_or_else(|| "/".to_string()),
        summary: text.chars().take(100).collect(),
        text,
        importance: importance.unwrap_or(0.7).clamp(0.0, 1.0),
        timestamp,
        category: "fact".to_string(),
        topic: String::new(),
        keywords: vec![],
        persons: vec![],
        entities: vec![],
        location: String::new(),
        source: "cli".to_string(),
        scope: "general".to_string(),
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata: json!({}),
        vector: None,
    }
}

fn evaluate_cli_capability_enabled(
    cap_type: &str,
    definition: &str,
) -> Result<(bool, Option<String>), Box<dyn std::error::Error>> {
    if cap_type != "mcp" {
        return Ok((true, None));
    }

    let def: serde_json::Value = serde_json::from_str(definition)?;
    let transport_type = def["transport"].as_str().unwrap_or("stdio");
    if transport_type != "stdio" {
        return Ok((true, None));
    }

    match def["command"].as_str() {
        Some(cmd) if is_trusted_command(cmd) => Ok((true, None)),
        Some(cmd) => Ok((
            false,
            Some(format!(
                "Command '{}' is not in the trusted allowlist. Capability registered but disabled.",
                cmd
            )),
        )),
        None => Ok((
            false,
            Some(
                "mcp definition missing 'command' for stdio transport. Capability registered but disabled."
                    .to_string(),
            ),
        )),
    }
}

fn run_cli_command(command: Commands, db_path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Commands::Serve => Ok(()),
        Commands::Search { query, path, top_k } => {
            let mut store = open_cli_store(db_path)?;
            let path_prefix = path.clone();
            let results = store.search(
                &query,
                Some(SearchOptions {
                    top_k,
                    path_prefix,
                    ..Default::default()
                }),
            )?;
            print_pretty_json(&json!({
                "query": query,
                "path": path,
                "top_k": top_k,
                "vector": store.vec_available,
                "results": results,
            }))
        }
        Commands::Save {
            text,
            path,
            importance,
        } => {
            if memory_core::is_noise_text(&text) {
                return print_pretty_json(&json!({
                    "saved": false,
                    "noise": true,
                    "reason": "Text detected as noise (greeting, denial, or meta-question). Not saved.",
                    "hint": "Retry with a non-noise sentence.",
                }));
            }

            let mut store = open_cli_store(db_path)?;
            let id = uuid::Uuid::new_v4().to_string();
            let timestamp = Utc::now().to_rfc3339();
            let entry =
                build_cli_memory_entry(id.clone(), text, path, importance, timestamp.clone());

            store.upsert(&entry)?;
            print_pretty_json(&json!({
                "saved": true,
                "id": id,
                "timestamp": timestamp,
                "path": entry.path,
                "importance": entry.importance,
            }))
        }
        Commands::Stats => {
            let store = open_cli_store(db_path)?;
            let stats = store.stats(false)?;
            print_pretty_json(&json!({
                "total": stats.total,
                "by_scope": stats.by_scope,
                "by_category": stats.by_category,
                "by_root_path": stats.by_root_path,
                "database": {
                    "path": db_path.display().to_string(),
                    "vec_available": store.vec_available,
                }
            }))
        }
        Commands::Gc => {
            let mut store = open_cli_store(db_path)?;
            print_pretty_json(&store.gc_tables()?)
        }
        Commands::Hub { action } => {
            let store = open_cli_store(db_path)?;
            match action {
                HubAction::List { cap_type } => {
                    let caps = store.hub_list(cap_type.as_deref(), false)?;
                    print_pretty_json(&serde_json::to_value(caps)?)
                }
                HubAction::Register {
                    id,
                    cap_type,
                    name,
                    definition,
                    description,
                } => {
                    let (enabled, warning) =
                        evaluate_cli_capability_enabled(&cap_type, &definition)?;
                    let cap = HubCapability {
                        id: id.clone(),
                        cap_type,
                        name,
                        version: 1,
                        description: description.unwrap_or_default(),
                        definition,
                        enabled,
                        uses: 0,
                        successes: 0,
                        failures: 0,
                        avg_rating: 0.0,
                        last_used: None,
                        created_at: String::new(),
                        updated_at: String::new(),
                    };
                    store.hub_register(&cap)?;
                    let saved = store.hub_get(&id)?.ok_or_else(|| {
                        std::io::Error::other(format!(
                            "capability '{}' registered but failed to reload from DB",
                            id
                        ))
                    })?;
                    let mut output = serde_json::to_value(saved)?;
                    if let Some(w) = warning {
                        if let Some(obj) = output.as_object_mut() {
                            obj.insert("warning".to_string(), json!(w));
                        }
                    }
                    print_pretty_json(&output)
                }
                HubAction::Enable { id } => {
                    let updated = store.hub_set_enabled(&id, true)?;
                    print_pretty_json(&json!({
                        "updated": updated,
                        "id": id,
                        "enabled": true,
                    }))
                }
                HubAction::Disable { id } => {
                    let updated = store.hub_set_enabled(&id, false)?;
                    print_pretty_json(&json!({
                        "updated": updated,
                        "id": id,
                        "enabled": false,
                    }))
                }
                HubAction::Stats => {
                    let caps = store.hub_list(None, false)?;
                    let mut by_type: HashMap<String, usize> = HashMap::new();
                    for cap in &caps {
                        *by_type.entry(cap.cap_type.clone()).or_insert(0) += 1;
                    }
                    let total_uses: u64 = caps.iter().map(|c| c.uses).sum();
                    let total_successes: u64 = caps.iter().map(|c| c.successes).sum();
                    print_pretty_json(&json!({
                        "total_capabilities": caps.len(),
                        "by_type": by_type,
                        "total_uses": total_uses,
                        "total_successes": total_successes,
                        "success_rate": if total_uses > 0 { total_successes as f64 / total_uses as f64 } else { 0.0 },
                    }))
                }
            }
        }
    }
}

pub(super) fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    tokio_main(cli)
}

#[tokio::main]
async fn tokio_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Load config from dotenv files (same as before)
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let expand_user_path = |raw: &str| {
        if raw == "~" {
            home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            home.join(rest)
        } else {
            PathBuf::from(raw)
        }
    };

    let app_home = std::env::var("TACHI_HOME")
        .or_else(|_| std::env::var("SIGIL_HOME"))
        .map(|v| expand_user_path(&v))
        .unwrap_or_else(|_| home.join(".tachi"));
    let git_root = find_git_root();

    let expand_cli_path = |raw: &PathBuf| expand_user_path(raw.to_string_lossy().as_ref());

    let _ = dotenvy::from_path(home.join(".secrets/master.env"));
    let _ = dotenvy::from_path_override(app_home.join("config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".tachi/config.env"));
    // Backward compatibility with old Sigil paths
    let _ = dotenvy::from_path_override(home.join(".sigil/config.env"));
    let _ = dotenvy::from_path_override(PathBuf::from(".sigil/config.env"));

    // Project-local dotenv support (non-overriding):
    // - current working directory .env
    // - git root .env (if different from cwd)
    if let Ok(cwd) = std::env::current_dir() {
        let _ = dotenvy::from_path(cwd.join(".env"));
        if let Some(root) = git_root.as_ref() {
            if root != &cwd {
                let _ = dotenvy::from_path(root.join(".env"));
            }
        }
    }

    let command = cli.command.clone().unwrap_or(Commands::Serve);

    // Resolve global DB path
    let global_db_path = if let Some(p) = cli.global_db.as_ref() {
        expand_cli_path(p)
    } else if let Ok(p) = std::env::var("MEMORY_DB_PATH") {
        expand_user_path(&p)
    } else {
        let default_global = app_home.join("global/memory.db");
        // Migration: move legacy DBs into ${TACHI_HOME}/global/memory.db
        let legacy_candidates = vec![
            app_home.join("memory.db"),
            home.join(".sigil/global/memory.db"),
            home.join(".sigil/memory.db"),
        ];
        if !default_global.exists() {
            for legacy in legacy_candidates {
                if legacy.exists() {
                    if let Some(parent) = default_global.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::copy(&legacy, &default_global).await?;
                    eprintln!(
                        "Migrated legacy DB: {} -> {}",
                        legacy.display(),
                        default_global.display()
                    );
                    break;
                }
            }
        }
        default_global
    };

    if let Some(parent) = global_db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if !matches!(command, Commands::Serve) {
        return run_cli_command(command, &global_db_path);
    }

    let gc_enabled = cli
        .gc_enabled
        .or_else(|| parse_env_bool("MEMORY_GC_ENABLED"))
        .unwrap_or(true);
    let gc_initial_delay_secs = cli
        .gc_initial_delay_secs
        .or_else(|| parse_env_u64("MEMORY_GC_INITIAL_DELAY_SECS"))
        .unwrap_or(300);
    let mut gc_interval_secs = cli
        .gc_interval_secs
        .or_else(|| parse_env_u64("MEMORY_GC_INTERVAL_SECS"))
        .unwrap_or(6 * 3600);
    if gc_interval_secs == 0 {
        eprintln!("MEMORY_GC_INTERVAL_SECS/--gc-interval-secs must be >= 1; using 1 second");
        gc_interval_secs = 1;
    }

    // Resolve project DB path
    let explicit_project_db = cli.project_db.is_some();
    let mut project_db_path = if cli.no_project_db {
        if cli.project_db.is_some() {
            eprintln!("--project-db is ignored because --no-project-db is set");
        }
        None
    } else if let Some(p) = cli.project_db.as_ref() {
        Some(expand_cli_path(p))
    } else if let Some(root) = git_root.as_ref() {
        let project_default = root.join(".tachi/memory.db");
        let project_legacy = root.join(".sigil/memory.db");

        if project_legacy.exists() && !project_default.exists() {
            if let Some(parent) = project_default.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&project_legacy, &project_default).await?;
            eprintln!(
                "Migrated legacy project DB: {} -> {}",
                project_legacy.display(),
                project_default.display()
            );
        }

        Some(project_default)
    } else {
        None
    };

    if cli.daemon && project_db_path.is_some() {
        if explicit_project_db {
            eprintln!(
                "Daemon mode: using explicit project DB path {} (single-project mode)",
                project_db_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string())
            );
        } else {
            eprintln!(
                "Daemon mode: auto-detected project DB disabled to avoid mixed project context. Use --project-db PATH to opt into single-project daemon mode."
            );
            project_db_path = None;
        }
    }

    // Ensure parent dirs exist
    if let Some(parent) = global_db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if let Some(ref p) = project_db_path {
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let server = MemoryServer::new(global_db_path.clone(), project_db_path.clone())?;

    // Spawn idle connection cleanup task
    {
        let pool = server.pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut conns = lock_or_recover(&pool.connections, "mcp_pool.connections");
                let now = Instant::now();
                let idle_ttl = pool.idle_ttl;
                let stale: Vec<String> = conns
                    .iter()
                    .filter(|(_, c)| now.duration_since(c.last_used) > idle_ttl)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in stale {
                    if let Some(conn) = conns.remove(&key) {
                        eprintln!("Idle cleanup: disconnecting '{}'", key);
                        drop(conn);
                    }
                }
            }
        });
    }

    if gc_enabled {
        eprintln!(
            "Background GC enabled (initial_delay={}s, interval={}s)",
            gc_initial_delay_secs, gc_interval_secs
        );
        let gc_server = server.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(gc_initial_delay_secs)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(gc_interval_secs));
            loop {
                interval.tick().await;
                eprintln!("[gc] Running scheduled garbage collection...");
                match gc_server
                    .with_global_store(|store| store.gc_tables().map_err(|e| format!("{}", e)))
                {
                    Ok(result) => eprintln!("[gc] Global DB: {}", result),
                    Err(e) => eprintln!("[gc] Global DB error: {}", e),
                }
                if gc_server.project_db_path.is_some() {
                    match gc_server
                        .with_project_store(|store| store.gc_tables().map_err(|e| format!("{}", e)))
                    {
                        Ok(result) => eprintln!("[gc] Project DB: {}", result),
                        Err(e) => eprintln!("[gc] Project DB error: {}", e),
                    }
                }
            }
        });
    } else {
        eprintln!("Background GC disabled");
    }

    // Integrity check on global DB
    server
        .with_global_store(|store| {
            match store.quick_check() {
                Ok(true) => eprintln!("Global database integrity: OK"),
                Ok(false) => eprintln!("WARNING: Global database may be corrupted!"),
                Err(e) => eprintln!("WARNING: Could not check global database integrity: {e}"),
            }
            Ok(())
        })
        .map_err(|e| format!("startup check: {e}"))?;

    // Integrity check on project DB
    if project_db_path.is_some() {
        server
            .with_project_store(|store| {
                match store.quick_check() {
                    Ok(true) => eprintln!("Project database integrity: OK"),
                    Ok(false) => eprintln!("WARNING: Project database may be corrupted!"),
                    Err(e) => {
                        eprintln!("WARNING: Could not check project database integrity: {e}")
                    }
                }
                Ok(())
            })
            .map_err(|e| format!("startup check: {e}"))?;
    }

    // Load cached proxy tools from Hub
    {
        let load_proxy_tools = |store: &mut MemoryStore| -> Result<(), String> {
            let mcp_caps = store
                .hub_list(Some("mcp"), true)
                .map_err(|e| format!("hub list: {e}"))?;
            for cap in mcp_caps {
                let def: serde_json::Value = match serde_json::from_str(&cap.definition) {
                    Ok(def) => def,
                    Err(e) => {
                        eprintln!(
                            "[startup] skip MCP '{}' due to invalid definition JSON: {e}",
                            cap.id
                        );
                        continue;
                    }
                };
                if let Some(tools_json) = def.get("discovered_tools") {
                    match serde_json::from_value::<Vec<rmcp::model::Tool>>(tools_json.clone()) {
                        Ok(tools) => {
                            let server_name = cap.id.strip_prefix("mcp:").unwrap_or(&cap.id);
                            let filtered_tools = filter_mcp_tools_by_permissions(&def, tools);
                            lock_or_recover(&server.proxy_tools, "proxy_tools")
                                .insert(server_name.to_string(), filtered_tools);
                        }
                        Err(e) => {
                            eprintln!(
                                "[startup] skip cached tools for '{}' due to invalid tool payload: {e}",
                                cap.id
                            );
                        }
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = server.with_global_store(load_proxy_tools) {
            eprintln!("[startup] failed loading global MCP proxy cache: {e}");
        }
        if server.project_db_path.is_some() {
            if let Err(e) = server.with_project_store(load_proxy_tools) {
                eprintln!("[startup] failed loading project MCP proxy cache: {e}");
            }
        }
    }
    {
        let load_skill_tools = |store: &mut MemoryStore| -> Result<(), String> {
            let skill_caps = store
                .hub_list(Some("skill"), true)
                .map_err(|e| format!("hub list: {e}"))?;
            for cap in skill_caps {
                if should_expose_skill_tool(&cap) {
                    if let Err(e) = server.register_skill_tool(&cap) {
                        eprintln!(
                            "[startup] failed to register skill tool for '{}': {}",
                            cap.id, e
                        );
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = server.with_global_store(load_skill_tools) {
            eprintln!("[startup] failed loading global skill tools: {e}");
        }
        if server.project_db_path.is_some() {
            if let Err(e) = server.with_project_store(load_skill_tools) {
                eprintln!("[startup] failed loading project skill tools: {e}");
            }
        }
    }

    if server.pipeline_enabled {
        eprintln!("Pipeline workers: ENABLED (external)");
    } else {
        eprintln!("Pipeline workers: DISABLED (set ENABLE_PIPELINE=true to enable)");
    }

    eprintln!("Starting Tachi MCP Server v{}", env!("CARGO_PKG_VERSION"));
    eprintln!(
        "Transport: {}",
        if cli.daemon {
            format!("HTTP daemon on port {}", cli.port)
        } else {
            "stdio".to_string()
        }
    );
    eprintln!("Global DB: {}", global_db_path.display());
    if let Some(ref p) = project_db_path {
        eprintln!("Project DB: {}", p.display());
    } else {
        eprintln!("Project DB: none (not in a git repository)");
    }
    eprintln!(
        "Vector search: global={}, project={}",
        server.global_vec_available, server.project_vec_available
    );

    if cli.daemon {
        // HTTP daemon mode
        // In daemon mode, project DB auto-detection is disabled above to avoid
        // mixed project context. Users can still opt into single-project mode
        // via explicit --project-db.

        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };
        use tokio_util::sync::CancellationToken;

        let ct = CancellationToken::new();
        let ct_shutdown = ct.clone();
        let port = cli.port;

        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                cancellation_token: ct.child_token(),
                ..Default::default()
            },
        );

        let router = axum::Router::new().nest_service("/mcp", service);
        let bind_addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

        eprintln!("Tachi daemon listening on http://{bind_addr}");

        tokio::select! {
            result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await }) => {
                if let Err(e) = result {
                    eprintln!("HTTP server error: {e}");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Received SIGINT, shutting down gracefully...");
                ct.cancel();
            }
        }
    } else {
        // stdio mode (default)
        let transport = (stdin(), stdout());
        let running = rmcp::service::serve_server(server, transport).await?;

        // Graceful shutdown: wait for either MCP quit or SIGINT/SIGTERM
        tokio::select! {
            quit_reason = running.waiting() => {
                eprintln!("Memory MCP Server stopped: {:?}", quit_reason);
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("Received SIGINT, shutting down gracefully...");
            }
        }
    }

    Ok(())
}
