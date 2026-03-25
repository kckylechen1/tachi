use super::*;

fn capability_callable(cap: &HubCapability) -> bool {
    if !cap.enabled {
        return false;
    }
    if !cap.cap_type.eq_ignore_ascii_case("mcp") {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(&cap.definition) {
        Ok(def) => def
            .get("discovery_status")
            .and_then(|v| v.as_str())
            .map(|s| s == "ready")
            .unwrap_or(true),
        Err(_) => cap.enabled,
    }
}

fn risk_rank(risk: &str) -> u8 {
    match risk.trim().to_ascii_lowercase().as_str() {
        "high" => 3,
        "medium" => 2,
        _ => 1,
    }
}

fn normalize_risk(risk: &str) -> &'static str {
    match risk_rank(risk) {
        3 => "high",
        2 => "medium",
        _ => "low",
    }
}

fn findings_to_vec(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| match x {
                    serde_json::Value::String(s) => Some(s.trim().to_string()),
                    serde_json::Value::Object(map) => map
                        .get("finding")
                        .or_else(|| map.get("issue"))
                        .or_else(|| map.get("signal"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string()),
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

fn signals_to_vec(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| match x {
                    serde_json::Value::String(s) => Some(s.trim().to_string()),
                    serde_json::Value::Object(map) => map
                        .get("signal")
                        .or_else(|| map.get("id"))
                        .or_else(|| map.get("name"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string()),
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

fn merge_findings(static_findings: &[String], llm_findings: &[String]) -> Vec<String> {
    let mut merged = Vec::new();
    for item in static_findings.iter().chain(llm_findings.iter()) {
        if !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

fn scan_skill_definition(def: &serde_json::Value) -> serde_json::Value {
    let mut findings: Vec<String> = Vec::new();
    let mut signals: Vec<String> = Vec::new();
    let mut high_hits: u32 = 0;
    let mut medium_hits: u32 = 0;

    let prompt_like_fields = ["prompt", "template", "system", "instructions", "description"];
    let mut corpus = String::new();
    for field in prompt_like_fields {
        if let Some(text) = def.get(field).and_then(|v| v.as_str()) {
            if !corpus.is_empty() {
                corpus.push('\n');
            }
            corpus.push_str(text);
        }
    }
    let lower = corpus.to_ascii_lowercase();

    let mut record = |signal: &str, finding: &str, severity: &str| {
        if !signals.iter().any(|s| s == signal) {
            signals.push(signal.to_string());
        }
        if !findings.iter().any(|f| f == finding) {
            findings.push(finding.to_string());
        }
        if severity == "high" {
            high_hits += 1;
        } else {
            medium_hits += 1;
        }
    };

    let critical_patterns = [
        ("rm -rf", "destructive_action", "Contains destructive shell command pattern 'rm -rf'"),
        ("mkfs", "destructive_action", "Contains disk format pattern 'mkfs'"),
        ("dd if=", "destructive_action", "Contains raw disk write pattern 'dd if='"),
        ("shred ", "destructive_action", "Contains secure delete pattern 'shred'"),
        ("sudo ", "privilege_escalation", "Contains privileged command pattern 'sudo'"),
        (
            "curl | sh",
            "remote_bootstrap",
            "Contains remote shell pipe pattern 'curl | sh'",
        ),
        (
            "curl|sh",
            "remote_bootstrap",
            "Contains remote shell pipe pattern 'curl|sh'",
        ),
        (
            "wget | sh",
            "remote_bootstrap",
            "Contains remote shell pipe pattern 'wget | sh'",
        ),
        (
            "wget|sh",
            "remote_bootstrap",
            "Contains remote shell pipe pattern 'wget|sh'",
        ),
        (
            "invoke-expression",
            "remote_bootstrap",
            "Contains PowerShell remote execution pattern 'Invoke-Expression'",
        ),
        (
            "begin rsa private key",
            "secret_exposure",
            "Contains private key material marker",
        ),
        (
            "begin openssh private key",
            "secret_exposure",
            "Contains OpenSSH private key material marker",
        ),
        (
            "aws_secret_access_key",
            "secret_exposure",
            "Contains inline AWS secret key marker",
        ),
        (
            "ghp_",
            "secret_exposure",
            "Contains inline GitHub token marker",
        ),
    ];
    for (pat, signal, msg) in critical_patterns {
        if lower.contains(pat) {
            record(signal, msg, "high");
        }
    }

    let warning_patterns = [
        ("os.system(", "unbounded_execution", "Contains os.system execution pattern"),
        (
            "subprocess.",
            "unbounded_execution",
            "Contains subprocess execution pattern",
        ),
        ("eval(", "unbounded_execution", "Contains eval execution pattern"),
        ("exec(", "unbounded_execution", "Contains exec execution pattern"),
        ("bash -c", "unbounded_execution", "Contains shell trampoline pattern 'bash -c'"),
        ("sh -c", "unbounded_execution", "Contains shell trampoline pattern 'sh -c'"),
        (
            "process.env",
            "secret_exposure",
            "Contains process.env environment access pattern",
        ),
        (
            "printenv",
            "secret_exposure",
            "Contains environment dump pattern 'printenv'",
        ),
        (
            ".env",
            "secret_exposure",
            "Contains '.env' access indicator",
        ),
        (
            "~/.ssh",
            "secret_exposure",
            "Contains '~/.ssh' key path indicator",
        ),
        (
            "/etc/passwd",
            "data_exfiltration",
            "Contains local system file path '/etc/passwd'",
        ),
        (
            "ignore previous instructions",
            "prompt_injection",
            "Contains prompt override phrase 'ignore previous instructions'",
        ),
        (
            "ignore all previous",
            "prompt_injection",
            "Contains prompt override phrase 'ignore all previous'",
        ),
        (
            "bypass safety",
            "prompt_injection",
            "Contains policy bypass phrase 'bypass safety'",
        ),
        (
            "disable safety",
            "prompt_injection",
            "Contains policy bypass phrase 'disable safety'",
        ),
        (
            "reveal system prompt",
            "prompt_injection",
            "Contains prompt extraction phrase 'reveal system prompt'",
        ),
        (
            "exfiltrate",
            "data_exfiltration",
            "Contains explicit exfiltration keyword",
        ),
        (
            "webhook",
            "data_exfiltration",
            "Contains webhook external delivery pattern",
        ),
    ];
    for (pat, signal, msg) in warning_patterns {
        if lower.contains(pat) {
            record(signal, msg, "medium");
        }
    }

    let risk = if high_hits > 0 {
        "high"
    } else if medium_hits > 0 {
        "medium"
    } else {
        "low"
    };

    serde_json::json!({
        "scanned_at": Utc::now().to_rfc3339(),
        "risk": risk,
        "blocked": risk == "high",
        "signals": signals,
        "findings": findings,
        "engine": "static-heuristic-v2",
        "high_hits": high_hits,
        "medium_hits": medium_hits
    })
}

async fn scan_skill_definition_with_llm(
    server: &MemoryServer,
    def: &serde_json::Value,
) -> Option<serde_json::Value> {
    if cfg!(test) {
        return None;
    }
    let enabled = parse_env_bool("SKILL_SECURITY_SCAN_USE_LLM").unwrap_or(true);
    if !enabled {
        return None;
    }

    let model = std::env::var("SKILL_SECURITY_SCAN_MODEL")
        .unwrap_or_else(|_| "Qwen/Qwen3.5-27B".to_string());
    let payload = serde_json::to_string(def).ok()?;

    match server
        .llm
        .call_llm(
            crate::prompts::SKILL_SECURITY_SCAN_PROMPT,
            &payload,
            Some(&model),
            0.1,
            800,
        )
        .await
    {
        Ok(raw) => {
            let parsed: serde_json::Value = serde_json::from_str(llm::LlmClient::strip_code_fence(&raw))
                .unwrap_or_else(|_| serde_json::json!({
                    "risk": "medium",
                    "blocked": false,
                    "findings": ["Failed to parse LLM security scan JSON output"],
                    "reason": raw
                }));
            Some(serde_json::json!({
                "status": "ok",
                "model": model,
                "result": parsed,
            }))
        }
        Err(e) => Some(serde_json::json!({
            "status": "error",
            "model": model,
            "error": e,
        })),
    }
}

fn merge_skill_scans(
    static_scan: &serde_json::Value,
    llm_scan: Option<&serde_json::Value>,
) -> serde_json::Value {
    let static_risk = static_scan
        .get("risk")
        .and_then(|v| v.as_str())
        .unwrap_or("low");
    let static_blocked = static_scan
        .get("blocked")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let static_findings = findings_to_vec(static_scan.get("findings"));
    let static_signals = merge_findings(
        &signals_to_vec(static_scan.get("signals")),
        &signals_to_vec(static_scan.get("dangerous_signals")),
    );

    let mut llm_risk = "low";
    let mut llm_blocked = false;
    let mut llm_findings: Vec<String> = Vec::new();
    let mut llm_signals: Vec<String> = Vec::new();
    let mut llm_status = "skipped".to_string();
    let mut llm_meta = serde_json::json!({});

    if let Some(scan) = llm_scan {
        llm_status = scan
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        llm_meta = scan.clone();

        if llm_status == "ok" {
            if let Some(result) = scan.get("result") {
                llm_risk = result
                    .get("risk")
                    .and_then(|v| v.as_str())
                    .unwrap_or("low");
                llm_blocked = result
                    .get("blocked")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                llm_findings = findings_to_vec(result.get("findings"));
                llm_signals = merge_findings(
                    &signals_to_vec(result.get("signals")),
                    &signals_to_vec(result.get("dangerous_signals")),
                );
            }
        }
    }

    let final_risk = if risk_rank(llm_risk) > risk_rank(static_risk) {
        llm_risk
    } else {
        static_risk
    };
    let blocked = static_blocked || llm_blocked || normalize_risk(final_risk) == "high";
    let findings = merge_findings(&static_findings, &llm_findings);
    let signals = merge_findings(&static_signals, &llm_signals);

    serde_json::json!({
        "scanned_at": Utc::now().to_rfc3339(),
        "risk": normalize_risk(final_risk),
        "blocked": blocked,
        "signals": signals,
        "findings": findings,
        "engine": "hybrid-static-llm-v1",
        "static": static_scan,
        "llm": llm_meta,
        "llm_status": llm_status
    })
}

pub(super) async fn handle_hub_register(
    server: &MemoryServer,
    params: HubRegisterParams,
) -> Result<String, String> {
    let (target_db, warning) = server.resolve_write_scope(&params.scope);

    let mut resp = serde_json::Map::new();
    resp.insert("id".into(), json!(params.id));
    resp.insert("db".into(), json!(target_db.as_str()));
    resp.insert("version".into(), json!(params.version));
    if let Some(w) = warning {
        append_warning(&mut resp, w);
    }

    let mut cap_definition = params.definition.clone();
    let mut enabled = true;

    if params.cap_type == "mcp" {
        let mut def: serde_json::Value = serde_json::from_str(&params.definition)
            .map_err(|e| format!("invalid mcp definition JSON: {e}"))?;
        let transport_type = def["transport"].as_str().unwrap_or("stdio").to_string();
        let tool_exposure_mode = resolve_mcp_tool_exposure(&def, server.mcp_tool_exposure_mode);
        def["tool_exposure"] = json!(tool_exposure_mode.as_str());
        resp.insert("tool_exposure".into(), json!(tool_exposure_mode.as_str()));

        // Security: validate MCP server commands against allowlist
        let auto_enabled = if transport_type == "stdio" {
            if let Some(cmd) = def["command"].as_str() {
                is_trusted_command(cmd)
            } else {
                false
            }
        } else {
            true // SSE/HTTP are URLs, no local exec risk
        };

        let server_name = params
            .id
            .strip_prefix("mcp:")
            .unwrap_or(&params.id)
            .to_string();
        server.clear_proxy_tools(&server_name);

        if !auto_enabled {
            enabled = false;
            clear_mcp_discovery_metadata(&mut def);
            let cmd = def["command"].as_str().unwrap_or("unknown");
            append_warning(
                &mut resp,
                format!(
                    "Command '{}' is not in the trusted allowlist. Capability registered but disabled. Use hub_set_enabled to activate after review.",
                    cmd
                ),
            );
            resp.insert("enabled".into(), json!(false));
            resp.insert("discovery".into(), json!("skipped (capability disabled)"));
        } else {
            match server.discover_mcp_tools(&params.id, &def).await {
                Ok(tools) => {
                    let total_tools = tools.len();
                    let filtered_tools = filter_mcp_tools_by_permissions(&def, tools);
                    let tools_discovered = filtered_tools.len();
                    let tools_filtered_out = total_tools.saturating_sub(tools_discovered);

                    set_mcp_discovery_success(&mut def, &filtered_tools);
                    server.cache_proxy_tools(&server_name, filtered_tools);

                    resp.insert("enabled".into(), json!(true));
                    resp.insert("tools_discovered".into(), json!(tools_discovered));
                    resp.insert("tools_total".into(), json!(total_tools));
                    if tools_filtered_out > 0 {
                        resp.insert("tools_filtered_out".into(), json!(tools_filtered_out));
                    }
                    if total_tools > 0 && tools_discovered == 0 {
                        append_warning(
                            &mut resp,
                            "MCP discovery succeeded, but all tools were filtered by permissions",
                        );
                    }
                    if tool_exposure_mode == McpToolExposureMode::Gateway {
                        append_warning(
                            &mut resp,
                            "Direct server__tool exposure is disabled (tool_exposure=gateway); call child tools via hub_call",
                        );
                    }
                }
                Err(discovery_error) => {
                    enabled = false;
                    set_mcp_discovery_failure(&mut def, &discovery_error);
                    server.clear_proxy_tools(&server_name);

                    resp.insert("enabled".into(), json!(false));
                    resp.insert("discovery_error".into(), json!(discovery_error.clone()));
                    append_warning(
                        &mut resp,
                        "MCP discovery failed; capability registered as disabled. Fix config and re-register to recover.",
                    );
                }
            }
        }

        cap_definition = serde_json::to_string(&def)
            .map_err(|e| format!("Failed to serialize MCP definition: {e}"))?;
    }

    if params.cap_type == "skill" {
        let mut def: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&cap_definition) {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                append_warning(
                    &mut resp,
                    "Skill definition is not a JSON object; skipped static skill scan",
                );
                serde_json::json!({})
            }
            Err(_) => {
                append_warning(
                    &mut resp,
                    "Skill definition is not valid JSON; skipped static skill scan",
                );
                serde_json::json!({})
            }
        };

        if def.is_object() {
            let static_scan = scan_skill_definition(&def);
            let llm_scan = scan_skill_definition_with_llm(server, &def).await;
            let scan = merge_skill_scans(&static_scan, llm_scan.as_ref());

            let blocked = scan
                .get("blocked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let risk = scan
                .get("risk")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            def["security_scan"] = scan.clone();
            resp.insert("skill_scan".into(), scan);
            if blocked {
                enabled = false;
                resp.insert("enabled".into(), json!(false));
                append_warning(
                    &mut resp,
                    format!(
                        "Skill static scan blocked registration activation (risk={risk}). Review definition and re-enable explicitly."
                    ),
                );
            }
            cap_definition = serde_json::to_string(&def)
                .map_err(|e| format!("Failed to serialize skill definition: {e}"))?;
        }
    }

    let cap = HubCapability {
        id: params.id.clone(),
        cap_type: params.cap_type.clone(),
        name: params.name.clone(),
        version: params.version,
        description: params.description.clone(),
        definition: cap_definition,
        enabled,
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: String::new(),
        updated_at: String::new(),
    };
    let visibility = capability_visibility_for_cap(&cap);
    resp.insert("visibility".into(), json!(visibility.as_str()));
    resp.insert("callable".into(), json!(capability_callable(&cap)));

    server.with_store_for_scope(target_db, |store| {
        store
            .hub_register(&cap)
            .map_err(|e| format!("Failed to register: {e}"))
    })?;

    if params.cap_type == "skill" {
        if should_expose_skill_tool(&cap) {
            match server.register_skill_tool(&cap) {
                Ok(tool_name) => {
                    resp.insert("tool_name".into(), json!(tool_name));
                }
                Err(e) => {
                    resp.insert("skill_error".into(), json!(e));
                }
            }
        } else {
            let _ = server.unregister_skill_tool(&cap.id);
            append_warning(
                &mut resp,
                "Skill registered but not listed in tools (policy.visibility != 'listed'). Use run_skill or change policy.visibility.",
            );
        }

        // L0 analysis: async background scan of the prompt template
        let def: serde_json::Value = match serde_json::from_str(&params.definition) {
            Ok(def) => def,
            Err(e) => {
                append_warning(
                    &mut resp,
                    format!("Skill definition is not valid JSON; skipped async analysis ({e})"),
                );
                json!({})
            }
        };
        if let Some(prompt_text) = def.get("prompt").and_then(|v| v.as_str()) {
            let llm = server.llm.clone();
            let cap_clone = cap.clone();
            let desc_empty = params.description.is_empty();
            let db_path = match target_db {
                DbScope::Global => server.global_db_path.clone(),
                DbScope::Project => server
                    .project_db_path
                    .clone()
                    .unwrap_or_else(|| server.global_db_path.clone()),
            };
            let prompt_text = prompt_text.to_string();

            let cap_id = cap_clone.id.clone();

            tokio::spawn(async move {
                match llm
                    .call_llm(
                        crate::prompts::SKILL_ANALYSIS_PROMPT,
                        &prompt_text,
                        None,
                        0.3,
                        500,
                    )
                    .await
                {
                    Ok(analysis_raw) => {
                        let analysis_json: serde_json::Value = match serde_json::from_str(
                            llm::LlmClient::strip_code_fence(&analysis_raw),
                        ) {
                            Ok(parsed) => parsed,
                            Err(e) => {
                                eprintln!(
                                        "[skill-analysis] invalid JSON output for {}: {}; using raw summary fallback",
                                        cap_id, e
                                    );
                                serde_json::json!({"summary": analysis_raw})
                            }
                        };

                        // Auto-fill description if it was empty
                        if desc_empty {
                            if let Some(summary) = analysis_json["summary"].as_str() {
                                let mut updated_cap = cap_clone;
                                updated_cap.description = summary.to_string();
                                let db_str = db_path.to_string_lossy();
                                match MemoryStore::open(db_str.as_ref()) {
                                    Ok(store) => {
                                        if let Err(e) = store.hub_register(&updated_cap) {
                                            eprintln!(
                                                "[skill-analysis] failed to persist auto description for {}: {}",
                                                cap_id, e
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "[skill-analysis] failed to open DB for {} at '{}': {}",
                                            cap_id,
                                            db_path.display(),
                                            e
                                        );
                                    }
                                }
                            }
                        }
                        eprintln!("[skill-analysis] {}: {:?}", cap_id, analysis_json);
                    }
                    Err(e) => {
                        eprintln!("[skill-analysis] failed for {}: {}", cap_id, e);
                    }
                }
            });
            resp.insert("analysis".into(), json!("pending (async)"));
        }
    }

    serde_json::to_string(&serde_json::Value::Object(resp)).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_discover(
    server: &MemoryServer,
    params: HubDiscoverParams,
) -> Result<String, String> {
    let cap_type = params.cap_type.as_deref();

    let global_caps = server.with_global_store(|store| {
        if let Some(ref q) = params.query {
            store
                .hub_search(q, cap_type)
                .map_err(|e| format!("hub search global: {e}"))
        } else {
            store
                .hub_list(cap_type, params.enabled_only)
                .map_err(|e| format!("hub list global: {e}"))
        }
    })?;

    let project_caps = if server.project_db_path.is_some() {
        server.with_project_store(|store| {
            if let Some(ref q) = params.query {
                store
                    .hub_search(q, cap_type)
                    .map_err(|e| format!("hub search project: {e}"))
            } else {
                store
                    .hub_list(cap_type, params.enabled_only)
                    .map_err(|e| format!("hub list project: {e}"))
            }
        })?
    } else {
        vec![]
    };

    let mut seen = HashSet::new();
    let mut output: Vec<serde_json::Value> = Vec::new();

    for cap in &project_caps {
        seen.insert(cap.id.clone());
        let mut obj = serde_json::to_value(cap).unwrap_or(json!(null));
        if let Some(o) = obj.as_object_mut() {
            o.insert("db".into(), json!("project"));
            o.insert(
                "visibility".into(),
                json!(capability_visibility_for_cap(cap).as_str()),
            );
            o.insert("callable".into(), json!(capability_callable(cap)));
        }
        output.push(obj);
    }
    for cap in &global_caps {
        if !seen.insert(cap.id.clone()) {
            continue;
        }
        let mut obj = serde_json::to_value(cap).unwrap_or(json!(null));
        if let Some(o) = obj.as_object_mut() {
            o.insert("db".into(), json!("global"));
            o.insert(
                "visibility".into(),
                json!(capability_visibility_for_cap(cap).as_str()),
            );
            o.insert("callable".into(), json!(capability_callable(cap)));
        }
        output.push(obj);
    }

    serde_json::to_string(&output).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_get(
    server: &MemoryServer,
    params: HubGetParams,
) -> Result<String, String> {
    if server.project_db_path.is_some() {
        if let Some(cap) = server.with_project_store(|store| {
            store
                .hub_get(&params.id)
                .map_err(|e| format!("hub get project: {e}"))
        })? {
            let mut obj = serde_json::to_value(&cap).unwrap_or(json!(null));
            if let Some(o) = obj.as_object_mut() {
                o.insert("db".into(), json!("project"));
                o.insert(
                    "visibility".into(),
                    json!(capability_visibility_for_cap(&cap).as_str()),
                );
                o.insert("callable".into(), json!(capability_callable(&cap)));
            }
            return serde_json::to_string(&obj).map_err(|e| format!("serialize: {e}"));
        }
    }
    match server.with_global_store(|store| {
        store
            .hub_get(&params.id)
            .map_err(|e| format!("hub get global: {e}"))
    })? {
        Some(cap) => {
            let mut obj = serde_json::to_value(&cap).unwrap_or(json!(null));
            if let Some(o) = obj.as_object_mut() {
                o.insert("db".into(), json!("global"));
                o.insert(
                    "visibility".into(),
                    json!(capability_visibility_for_cap(&cap).as_str()),
                );
                o.insert("callable".into(), json!(capability_callable(&cap)));
            }
            serde_json::to_string(&obj).map_err(|e| format!("serialize: {e}"))
        }
        None => serde_json::to_string(&json!({"error": "Capability not found"}))
            .map_err(|e| format!("serialize: {e}")),
    }
}

pub(super) async fn handle_hub_feedback(
    server: &MemoryServer,
    params: HubFeedbackParams,
) -> Result<String, String> {
    if server.project_db_path.is_some() {
        let found = server.with_project_store(|store| {
            store
                .hub_get(&params.id)
                .map_err(|e| format!("hub get: {e}"))
        })?;
        if found.is_some() {
            server.with_project_store(|store| {
                store
                    .hub_record_feedback(&params.id, params.success, params.rating)
                    .map_err(|e| format!("feedback: {e}"))
            })?;
            return serde_json::to_string(
                &json!({"id": params.id, "recorded": true, "db": "project"}),
            )
            .map_err(|e| format!("serialize: {e}"));
        }
    }
    server.with_global_store(|store| {
        store
            .hub_record_feedback(&params.id, params.success, params.rating)
            .map_err(|e| format!("feedback: {e}"))
    })?;
    serde_json::to_string(&json!({"id": params.id, "recorded": true, "db": "global"}))
        .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_stats(server: &MemoryServer) -> Result<String, String> {
    let global_caps = server.with_global_store(|store| {
        store
            .hub_list(None, false)
            .map_err(|e| format!("hub list: {e}"))
    })?;
    let project_caps = if server.project_db_path.is_some() {
        server.with_project_store(|store| {
            store
                .hub_list(None, false)
                .map_err(|e| format!("hub list: {e}"))
        })?
    } else {
        vec![]
    };

    let total = global_caps.len() + project_caps.len();
    let mut by_type: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let all_caps: Vec<&HubCapability> = global_caps.iter().chain(project_caps.iter()).collect();
    for cap in &all_caps {
        *by_type.entry(cap.cap_type.clone()).or_insert(0) += 1;
    }
    let total_uses: u64 = all_caps.iter().map(|c| c.uses).sum();
    let total_successes: u64 = all_caps.iter().map(|c| c.successes).sum();

    serde_json::to_string(&json!({
        "total_capabilities": total,
        "by_type": by_type,
        "total_uses": total_uses,
        "total_successes": total_successes,
        "success_rate": if total_uses > 0 { total_successes as f64 / total_uses as f64 } else { 0.0 },
        "global_count": global_caps.len(),
        "project_count": project_caps.len(),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_set_enabled(
    server: &MemoryServer,
    params: HubSetEnabledParams,
) -> Result<String, String> {
    let mut updated = false;
    let mut target_db = "not_found";

    if server.project_db_path.is_some() {
        let result = server.with_project_store(|store| {
            store
                .hub_set_enabled(&params.id, params.enabled)
                .map_err(|e| format!("Failed: {}", e))
        })?;
        if result {
            updated = true;
            target_db = "project";
        }
    }

    if !updated {
        let result = server.with_global_store(|store| {
            store
                .hub_set_enabled(&params.id, params.enabled)
                .map_err(|e| format!("Failed: {}", e))
        })?;
        if result {
            updated = true;
            target_db = "global";
        }
    }

    if updated {
        if let Some(server_name) = params.id.strip_prefix("mcp:") {
            if params.enabled {
                if let Ok(cap) = server.get_capability(&params.id) {
                    match serde_json::from_str::<serde_json::Value>(&cap.definition) {
                        Ok(def) => {
                            if let Some(tools_json) = def.get("discovered_tools") {
                                match serde_json::from_value::<Vec<rmcp::model::Tool>>(
                                    tools_json.clone(),
                                ) {
                                    Ok(tools) => {
                                        server.cache_proxy_tools(
                                            server_name,
                                            filter_mcp_tools_by_permissions(&def, tools),
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "[hub_set_enabled] invalid discovered_tools payload for '{}': {}",
                                            params.id, e
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[hub_set_enabled] invalid definition JSON for '{}': {}",
                                params.id, e
                            );
                        }
                    }
                }
            } else {
                server.clear_proxy_tools(server_name);
            }
        }

        if params.id.starts_with("skill:") {
            if params.enabled {
                if let Ok(cap) = server.get_capability(&params.id) {
                    if should_expose_skill_tool(&cap) {
                        let _ = server.register_skill_tool(&cap);
                    } else {
                        let _ = server.unregister_skill_tool(&cap.id);
                    }
                }
            } else {
                let _ = server.unregister_skill_tool(&params.id);
            }
        }
    }

    serde_json::to_string(&json!({
        "updated": updated,
        "db": target_db,
        "id": params.id,
        "enabled": params.enabled,
    }))
    .map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_run_skill(
    server: &MemoryServer,
    params: RunSkillParams,
) -> Result<String, String> {
    let cap = {
        let mut found = None;
        if server.project_db_path.is_some() {
            found = server.with_project_store(|store| {
                store
                    .hub_get(&params.skill_id)
                    .map_err(|e| format!("hub get project: {e}"))
            })?;
        }
        if found.is_none() {
            found = server.with_global_store(|store| {
                store
                    .hub_get(&params.skill_id)
                    .map_err(|e| format!("hub get global: {e}"))
            })?;
        }
        found.ok_or_else(|| format!("Skill '{}' not found in Hub", params.skill_id))?
    };

    if cap.cap_type != "skill" {
        return Err(format!(
            "'{}' is type '{}', not 'skill'",
            params.skill_id, cap.cap_type
        ));
    }

    let def: serde_json::Value = serde_json::from_str(&cap.definition)
        .map_err(|e| format!("invalid skill definition JSON: {e}"))?;

    let prompt_template = def["prompt"]
        .as_str()
        .ok_or_else(|| "skill definition missing 'prompt' field".to_string())?;

    let mut resolved_prompt = prompt_template.to_string();
    if let Some(args_obj) = params.args.as_object() {
        for (k, v) in args_obj {
            let placeholder = format!("{{{{{}}}}}", k);
            let val_str = value_to_template_text(v);
            resolved_prompt = resolved_prompt.replace(&placeholder, &val_str);
        }
    }

    server
        .llm
        .call_llm(
            "You are an AI assistant executing a specialized skill.",
            &resolved_prompt,
            None,
            0.3,
            4000,
        )
        .await
        .map_err(|e| format!("skill execution failed: {}", e))
}

pub(super) async fn handle_tachi_audit_log(
    server: &MemoryServer,
    params: AuditLogParams,
) -> Result<String, String> {
    server.with_global_store(|store| {
        let entries = store
            .audit_log_list(params.limit, params.server_filter.as_deref())
            .map_err(|e| format!("audit log: {e}"))?;
        serde_json::to_string(&entries).map_err(|e| format!("serialize: {e}"))
    })
}

pub(super) async fn handle_hub_call(
    server: &MemoryServer,
    params: HubCallParams,
) -> Result<String, String> {
    // Check enabled status before calling
    let cap = server
        .get_capability(&params.server_id)
        .map_err(|e| format!("{e}"))?;
    if !cap.enabled {
        return Err(format!(
            "MCP server '{}' is disabled. Use hub_set_enabled to activate after review.",
            params.server_id
        ));
    }

    let server_name = params
        .server_id
        .strip_prefix("mcp:")
        .unwrap_or(&params.server_id);
    let result = server
        .proxy_call_internal(
            server_name,
            &params.tool_name,
            params.arguments.as_object().cloned(),
        )
        .await
        .map_err(|e| format!("{e}"))?;

    let content_texts: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| {
            serde_json::to_value(c)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str().map(String::from)))
        })
        .collect();
    serde_json::to_string(&json!({
        "server": params.server_id,
        "tool": params.tool_name,
        "content": content_texts,
        "is_error": result.is_error.unwrap_or(false),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_disconnect(
    server: &MemoryServer,
    params: HubDisconnectParams,
) -> Result<String, String> {
    let server_name = params
        .server_id
        .strip_prefix("mcp:")
        .unwrap_or(&params.server_id);

    // Remove from connection pool
    let had_connection = {
        let mut conns = server
            .pool
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        conns.remove(server_name).is_some()
    };

    // Also clear discovered tools cache
    {
        let mut tools = server.proxy_tools.lock().unwrap_or_else(|e| e.into_inner());
        tools.remove(server_name);
    }

    serde_json::to_string(&json!({
        "server": server_name,
        "disconnected": had_connection,
        "message": if had_connection {
            "Connection dropped. Next hub_call will reconnect with latest config."
        } else {
            "No active connection found (will connect fresh on next hub_call)."
        },
    }))
    .map_err(|e| format!("serialize: {e}"))
}
