use super::*;
use crate::hub_helpers::health_status_allows_call;

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

fn normalize_review_status(status: &str) -> Option<&'static str> {
    match status.trim().to_ascii_lowercase().as_str() {
        "pending" => Some("pending"),
        "approved" => Some("approved"),
        "rejected" => Some("rejected"),
        _ => None,
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
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for item in static_findings.iter().chain(llm_findings.iter()) {
        if seen.insert(item.as_str()) {
            merged.push(item.clone());
        }
    }
    merged
}

fn scan_skill_definition(def: &serde_json::Value) -> serde_json::Value {
    let mut findings: Vec<String> = Vec::new();
    let mut signals: Vec<String> = Vec::new();
    let mut seen_findings: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_signals: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut high_hits: u32 = 0;
    let mut medium_hits: u32 = 0;

    let prompt_like_fields = [
        "prompt",
        "template",
        "system",
        "instructions",
        "description",
    ];
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
        if seen_signals.insert(signal.to_string()) {
            signals.push(signal.to_string());
        }
        if seen_findings.insert(finding.to_string()) {
            findings.push(finding.to_string());
        }
        if severity == "high" {
            high_hits += 1;
        } else {
            medium_hits += 1;
        }
    };

    let critical_patterns = [
        (
            "rm -rf",
            "destructive_action",
            "Contains destructive shell command pattern 'rm -rf'",
        ),
        (
            "mkfs",
            "destructive_action",
            "Contains disk format pattern 'mkfs'",
        ),
        (
            "dd if=",
            "destructive_action",
            "Contains raw disk write pattern 'dd if='",
        ),
        (
            "shred ",
            "destructive_action",
            "Contains secure delete pattern 'shred'",
        ),
        (
            "sudo ",
            "privilege_escalation",
            "Contains privileged command pattern 'sudo'",
        ),
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
        (
            "os.system(",
            "unbounded_execution",
            "Contains os.system execution pattern",
        ),
        (
            "subprocess.",
            "unbounded_execution",
            "Contains subprocess execution pattern",
        ),
        (
            "eval(",
            "unbounded_execution",
            "Contains eval execution pattern",
        ),
        (
            "exec(",
            "unbounded_execution",
            "Contains exec execution pattern",
        ),
        (
            "bash -c",
            "unbounded_execution",
            "Contains shell trampoline pattern 'bash -c'",
        ),
        (
            "sh -c",
            "unbounded_execution",
            "Contains shell trampoline pattern 'sh -c'",
        ),
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
            let parsed: serde_json::Value =
                serde_json::from_str(llm::LlmClient::strip_code_fence(&raw)).unwrap_or_else(|_| {
                    serde_json::json!({
                        "risk": "medium",
                        "blocked": false,
                        "findings": ["Failed to parse LLM security scan JSON output"],
                        "reason": raw
                    })
                });
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
                llm_risk = result.get("risk").and_then(|v| v.as_str()).unwrap_or("low");
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
    let mut exposure_mode = "direct".to_string();

    if params.cap_type == "mcp" {
        let mut def: serde_json::Value = serde_json::from_str(&params.definition)
            .map_err(|e| format!("invalid mcp definition JSON: {e}"))?;
        let transport_type = def["transport"].as_str().unwrap_or("stdio").to_string();
        let tool_exposure_mode = resolve_mcp_tool_exposure(&def, server.mcp_tool_exposure_mode);
        exposure_mode = tool_exposure_mode.as_str().to_string();
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

        // Governance gate: MCP capabilities must be reviewed before activation.
        enabled = false;
        resp.insert("enabled".into(), json!(false));
        resp.insert("review_status".into(), json!("pending"));
        append_warning(
            &mut resp,
            "MCP capability registered in pending review state; use hub_review to approve before hub_call.",
        );
    }

    if params.cap_type == "skill" {
        let mut def: serde_json::Value =
            match serde_json::from_str::<serde_json::Value>(&cap_definition) {
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
        review_status: if params.cap_type == "mcp" {
            "pending".to_string()
        } else {
            "approved".to_string()
        },
        health_status: if params.cap_type == "mcp" {
            "unknown".to_string()
        } else {
            "healthy".to_string()
        },
        last_error: None,
        last_success_at: None,
        last_failure_at: None,
        fail_streak: 0,
        active_version: None,
        exposure_mode,
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
    let output = hub_discover_inner(server, &params)?;
    serde_json::to_string(&output).map_err(|e| format!("serialize: {e}"))
}

/// Shared inner function returning structured data to avoid double serde round-trips.
fn hub_discover_inner(
    server: &MemoryServer,
    params: &HubDiscoverParams,
) -> Result<Vec<serde_json::Value>, String> {
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

    let project_caps = if server.has_project_db() {
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

    Ok(output)
}

pub(super) async fn handle_hub_get(
    server: &MemoryServer,
    params: HubGetParams,
) -> Result<String, String> {
    if server.has_project_db() {
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
    if server.has_project_db() {
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
    let project_caps = if server.has_project_db() {
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

pub(super) async fn handle_hub_review(
    server: &MemoryServer,
    params: HubReviewParams,
) -> Result<String, String> {
    let review_status = normalize_review_status(&params.review_status)
        .ok_or_else(|| "review_status must be one of: pending, approved, rejected".to_string())?;

    let enabled_override = params.enabled.or_else(|| match review_status {
        "approved" => Some(true),
        "rejected" => Some(false),
        _ => None,
    });

    let mut updated = false;
    let mut target_db = "not_found";
    let mut cap: Option<HubCapability> = None;

    if server.has_project_db() {
        let in_project = server.with_project_store_read(|store| {
            store
                .hub_get(&params.id)
                .map(|c| c.is_some())
                .map_err(|e| format!("hub get project: {e}"))
        })?;
        if in_project {
            updated = server.with_project_store(|store| {
                store
                    .hub_set_review(&params.id, review_status, enabled_override)
                    .map_err(|e| format!("hub review project: {e}"))
            })?;
            if updated {
                target_db = "project";
                cap = server.with_project_store_read(|store| {
                    store
                        .hub_get(&params.id)
                        .map_err(|e| format!("hub get project: {e}"))
                })?;
            }
        }
    }

    if !updated {
        updated = server.with_global_store(|store| {
            store
                .hub_set_review(&params.id, review_status, enabled_override)
                .map_err(|e| format!("hub review global: {e}"))
        })?;
        if updated {
            target_db = "global";
            cap = server.with_global_store_read(|store| {
                store
                    .hub_get(&params.id)
                    .map_err(|e| format!("hub get global: {e}"))
            })?;
        }
    }

    if let Some(cap) = cap.as_ref() {
        if let Some(server_name) = cap.id.strip_prefix("mcp:") {
            if capability_callable(cap) {
                match serde_json::from_str::<serde_json::Value>(&cap.definition) {
                    Ok(def) => {
                        if let Some(tools_json) = def.get("discovered_tools") {
                            if let Ok(tools) =
                                serde_json::from_value::<Vec<rmcp::model::Tool>>(tools_json.clone())
                            {
                                server.cache_proxy_tools(
                                    server_name,
                                    filter_mcp_tools_by_permissions(&def, tools),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[hub_review] invalid definition JSON for '{}': {}",
                            cap.id, e
                        );
                    }
                }
            } else {
                server.clear_proxy_tools(server_name);
            }
        }

        if cap.id.starts_with("skill:") {
            if capability_callable(cap) && should_expose_skill_tool(cap) {
                let _ = server.register_skill_tool(cap);
            } else {
                let _ = server.unregister_skill_tool(&cap.id);
            }
        }
    }

    serde_json::to_string(&json!({
        "updated": updated,
        "db": target_db,
        "id": params.id,
        "review_status": review_status,
        "enabled": cap.as_ref().map(|c| c.enabled),
        "health_status": cap.as_ref().map(|c| c.health_status.clone()),
        "callable": cap.as_ref().map(capability_callable),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_set_active_version(
    server: &MemoryServer,
    params: HubSetActiveVersionParams,
) -> Result<String, String> {
    if params.alias_id.trim().is_empty() || params.active_capability_id.trim().is_empty() {
        return Err("alias_id and active_capability_id must be non-empty".to_string());
    }

    let active_cap = server
        .get_capability(&params.active_capability_id)
        .map_err(|e| format!("{e}"))?;

    let target_db = if server.has_project_db()
        && server.with_project_store_read(|store| {
            store
                .hub_get(&params.alias_id)
                .map(|cap| cap.is_some())
                .map_err(|e| format!("hub get project alias: {e}"))
        })? {
        DbScope::Project
    } else {
        DbScope::Global
    };

    server.with_store_for_scope(target_db, |store| {
        store
            .hub_set_active_version_route(&params.alias_id, &params.active_capability_id)
            .map_err(|e| format!("hub route set: {e}"))?;

        if let Some(mut alias_cap) = store
            .hub_get(&params.alias_id)
            .map_err(|e| format!("hub get alias: {e}"))?
        {
            alias_cap.active_version = Some(params.active_capability_id.clone());
            store
                .hub_register(&alias_cap)
                .map_err(|e| format!("hub update alias metadata: {e}"))?;
        }
        Ok(())
    })?;

    serde_json::to_string(&json!({
        "updated": true,
        "db": target_db.as_str(),
        "alias_id": params.alias_id,
        "active_capability_id": params.active_capability_id,
        "active_capability_type": active_cap.cap_type,
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vc_register(
    server: &MemoryServer,
    params: VirtualCapabilityRegisterParams,
) -> Result<String, String> {
    if !params.id.starts_with("vc:") {
        return Err("Virtual capability id must start with 'vc:'".to_string());
    }

    let scope = if params.scope == "global" {
        DbScope::Global
    } else if server.has_project_db() {
        DbScope::Project
    } else {
        DbScope::Global
    };

    // ── Cross-scope shadowing guard (I-2) ────────────────────────────────────
    // Prevent registering a VC in one scope when the same ID already exists in
    // the other scope. This avoids a subtle split-brain where bindings in the
    // shadowed scope become unreachable.
    let other_scope = match scope {
        DbScope::Project => Some(DbScope::Global),
        DbScope::Global if server.has_project_db() => Some(DbScope::Project),
        _ => None,
    };
    if let Some(other) = other_scope {
        let exists_in_other = server
            .with_store_for_scope(other, |store| {
                store
                    .hub_get(&params.id)
                    .map(|cap| cap.is_some())
                    .map_err(|e| format!("check other scope: {e}"))
            })
            .unwrap_or(false);
        if exists_in_other {
            return Err(format!(
                "Virtual capability '{}' already exists in {} scope. \
                 Cross-scope VC shadowing is not allowed — it would orphan existing bindings. \
                 Use the existing VC or remove it first with hub_review(enabled=false).",
                params.id,
                other.as_str()
            ));
        }
    }

    let definition = json!({
        "contract": params.contract,
        "routing_strategy": if params.routing_strategy.trim().is_empty() {
            "priority"
        } else {
            params.routing_strategy.as_str()
        },
        "tags": params.tags,
        "input_schema": params.input_schema,
    });

    let cap = HubCapability {
        id: params.id.clone(),
        cap_type: "virtual".to_string(),
        name: params.name,
        // VCs are auto-approved because they are logical routing abstractions, not executable
        // code. Security governance applies at the concrete backend level via sandbox policies
        // and the hub_review gate. The VC layer only resolves *which* backend to call.
        version: 1,
        description: params.description,
        definition: serde_json::to_string(&definition).map_err(|e| format!("serialize: {e}"))?,
        enabled: true,
        review_status: "approved".to_string(),
        health_status: "healthy".to_string(),
        last_error: None,
        last_success_at: None,
        last_failure_at: None,
        fail_streak: 0,
        active_version: None,
        exposure_mode: "gateway".to_string(),
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: String::new(),
        updated_at: String::new(),
    };

    server.with_store_for_scope(scope, |store| {
        store
            .hub_register(&cap)
            .map_err(|e| format!("vc register: {e}"))
    })?;

    serde_json::to_string(&json!({
        "registered": true,
        "db": scope.as_str(),
        "id": params.id,
        "cap_type": "virtual",
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vc_bind(
    server: &MemoryServer,
    params: VirtualCapabilityBindParams,
) -> Result<String, String> {
    let vc_cap = server
        .get_capability(&params.vc_id)
        .map_err(|e| format!("{e}"))?;
    if !vc_cap.cap_type.eq_ignore_ascii_case("virtual") {
        return Err(format!(
            "Capability '{}' is not type 'virtual'",
            params.vc_id
        ));
    }

    let target_cap = server
        .get_capability(&params.capability_id)
        .map_err(|e| format!("{e}"))?;
    if !target_cap.cap_type.eq_ignore_ascii_case("mcp") {
        return Err(format!(
            "Virtual Capability targets must be MCP capabilities, got '{}' for '{}'",
            target_cap.cap_type, params.capability_id
        ));
    }

    let target_db = if server.has_project_db()
        && server.with_project_store_read(|store| {
            store
                .hub_get(&params.vc_id)
                .map(|cap| cap.is_some())
                .map_err(|e| format!("hub get project vc: {e}"))
        })? {
        DbScope::Project
    } else {
        DbScope::Global
    };

    let binding = VirtualCapabilityBinding {
        vc_id: params.vc_id.clone(),
        capability_id: params.capability_id.clone(),
        priority: params.priority,
        version_pin: params.version_pin,
        enabled: params.enabled,
        metadata: params.metadata.unwrap_or_else(|| json!({})),
        created_at: String::new(),
        updated_at: String::new(),
    };

    server.with_store_for_scope(target_db, |store| {
        store
            .vc_upsert_binding(&binding)
            .map_err(|e| format!("vc bind: {e}"))
    })?;

    let resolution = server.resolve_virtual_capability_target(&params.vc_id).ok();

    serde_json::to_string(&json!({
        "updated": true,
        "db": target_db.as_str(),
        "binding": binding,
        "resolution": resolution.map(|(_, report)| report),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vc_list(
    server: &MemoryServer,
    mut params: HubDiscoverParams,
) -> Result<String, String> {
    params.cap_type = Some("virtual".to_string());
    let mut items = hub_discover_inner(server, &params)?;

    for item in &mut items {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let (bindings, binding_db) = server.get_virtual_capability_bindings(id)?;
        if let Some(obj) = item.as_object_mut() {
            obj.insert(
                "bindings".to_string(),
                serde_json::to_value(bindings).unwrap_or_else(|_| json!([])),
            );
            obj.insert("binding_db".to_string(), json!(binding_db));
        }
    }

    serde_json::to_string(&items).map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_vc_resolve(
    server: &MemoryServer,
    params: VirtualCapabilityResolveParams,
) -> Result<String, String> {
    let (resolved_id, report) = server.resolve_virtual_capability_target(&params.id)?;
    serde_json::to_string(&json!({
        "id": params.id,
        "resolved_id": resolved_id,
        "report": report,
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_hub_set_enabled(
    server: &MemoryServer,
    params: HubSetEnabledParams,
) -> Result<String, String> {
    let mut updated = false;
    let mut target_db = "not_found";

    if server.has_project_db() {
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
        if server.has_project_db() {
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

    let result = server
        .llm
        .call_llm(
            "You are an AI assistant executing a specialized skill.",
            &resolved_prompt,
            None,
            0.3,
            4000,
        )
        .await;

    // Record call outcome for skill telemetry (enables skill_evolve)
    let success = result.is_ok();
    let error_msg = result.as_ref().err().map(|e| format!("{e}"));
    let _ = server.record_capability_call_outcome(
        &params.skill_id,
        success,
        error_msg.as_deref(),
    );

    result.map_err(|e| format!("skill execution failed: {}", e))
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
    let target = server.resolve_call_target(&params.server_id)?;
    let resolved_server_id = target.resolved_id.clone();
    let cap = server
        .get_capability(&resolved_server_id)
        .map_err(|e| format!("{e}"))?;

    if !cap.enabled {
        return Err(format!(
            "MCP server '{}' is disabled. Use hub_set_enabled to activate after review.",
            resolved_server_id
        ));
    }
    if !review_status_allows_call(&cap.review_status) {
        return Err(format!(
            "Capability '{}' is not approved (review_status={}). Use hub_review first.",
            resolved_server_id, cap.review_status
        ));
    }
    if !health_status_allows_call(&cap.health_status) {
        return Err(format!(
            "Capability '{}' is circuit-open (health_status={}).",
            resolved_server_id, cap.health_status
        ));
    }
    if !cap.cap_type.eq_ignore_ascii_case("mcp") {
        return Err(format!(
            "Capability '{}' is type '{}', expected MCP.",
            resolved_server_id, cap.cap_type
        ));
    }

    let result = server
        .proxy_call_capability_internal(
            &resolved_server_id,
            Some(&target.requested_id),
            &params.tool_name,
            params.arguments.as_object().cloned(),
        )
        .await;

    let success = result.is_ok();
    let error_kind = result.as_ref().err().map(|e| format!("{e}"));
    if let Err(e) =
        server.record_capability_call_outcome(&resolved_server_id, success, error_kind.as_deref())
    {
        eprintln!(
            "[hub_call] failed to persist governance health for '{}': {}",
            resolved_server_id, e
        );
    }

    let result = result.map_err(|e| format!("{e}"))?;

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
        "requested_kind": target.requested_kind,
        "resolved_server": resolved_server_id,
        "resolution": target.resolution,
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

// ─── Hub Export Skills ───────────────────────────────────────────────────────

/// Export Hub skills to agent-specific file formats.
///
/// Supports:
/// - **claude**: `~/.tachi/skills/<name>/SKILL.md` + symlinks to `~/.claude/skills/<name>`
/// - **openclaw**: `~/.openclaw/plugins/tachi-skills.json` with hook configuration
/// - **cursor**: `.cursor/rules/<name>.mdc` rule files in the working directory
/// - **generic**: Raw SKILL.md files to a specified directory
pub(super) async fn handle_export_skills(
    server: &MemoryServer,
    params: ExportSkillsParams,
) -> Result<String, String> {
    use std::collections::HashSet;

    let agent = params.agent.to_ascii_lowercase();
    let vis_filter = params.visibility.to_ascii_lowercase();

    // ── 1. Collect all skills from global + project DBs ──────────────────────
    let global_skills = server.with_global_store(|store| {
        store
            .hub_list(Some("skill"), true)
            .map_err(|e| format!("list global skills: {e}"))
    })?;

    let project_skills = if server.has_project_db() {
        server.with_project_store(|store| {
            store
                .hub_list(Some("skill"), true)
                .map_err(|e| format!("list project skills: {e}"))
        })?
    } else {
        vec![]
    };

    // Project-scoped skills take priority (same dedup pattern as hub_discover)
    let mut seen = HashSet::new();
    let mut all_skills: Vec<HubCapability> = Vec::new();

    for cap in project_skills {
        seen.insert(cap.id.clone());
        all_skills.push(cap);
    }
    for cap in global_skills {
        if seen.insert(cap.id.clone()) {
            all_skills.push(cap);
        }
    }

    // ── 2. Filter by requested skill IDs ─────────────────────────────────────
    if let Some(ref ids) = params.skill_ids {
        let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        all_skills.retain(|c| id_set.contains(c.id.as_str()));
    }

    // ── 3. Filter by visibility ──────────────────────────────────────────────
    if vis_filter != "all" {
        all_skills.retain(|cap| {
            let vis = capability_visibility_for_cap(cap);
            match vis_filter.as_str() {
                "listed" => vis == CapabilityVisibility::Listed,
                "discoverable" => {
                    vis == CapabilityVisibility::Discoverable
                        || vis == CapabilityVisibility::Listed
                }
                _ => true,
            }
        });
    }

    // ── 4. Filter by owner_agent for agent-local skills ──────────────────────
    all_skills.retain(|cap| {
        if let Ok(def) = serde_json::from_str::<serde_json::Value>(&cap.definition) {
            let scope = def
                .pointer("/policy/scope")
                .and_then(|v| v.as_str())
                .unwrap_or("pack-shared");
            if scope == "agent-local" {
                let owner = def
                    .pointer("/policy/owner_agent")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // Agent-local skills are only exported to the owning agent
                return owner.is_empty() || owner == agent;
            }
        }
        true // non-agent-local skills are always included
    });

    if all_skills.is_empty() {
        return serde_json::to_string(&json!({
            "agent": agent,
            "exported": 0,
            "message": "No skills matched the filter criteria"
        }))
        .map_err(|e| format!("serialize: {e}"));
    }

    // ── 5. Dispatch to agent-specific exporter ───────────────────────────────
    match agent.as_str() {
        "claude" => export_for_claude(&all_skills, &params),
        "openclaw" => export_for_openclaw(&all_skills, &params),
        "cursor" => export_for_cursor(&all_skills, &params),
        "generic" => export_for_generic(&all_skills, &params),
        other => Err(format!(
            "Unsupported agent target '{other}'. Supported: claude, openclaw, cursor, generic"
        )),
    }
}

/// Extract skill name from ID (e.g. "skill:code-review" -> "code-review")
fn skill_name_from_id(id: &str) -> &str {
    id.strip_prefix("skill:").unwrap_or(id)
}

/// Extract the SKILL.md content from a capability's definition JSON.
fn skill_content(cap: &HubCapability) -> Option<String> {
    let def: serde_json::Value = serde_json::from_str(&cap.definition).ok()?;
    def.get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Export skills for Claude Code: write SKILL.md to ~/.tachi/skills/<name>/ and
/// create symlinks in ~/.claude/skills/.
fn export_for_claude(
    skills: &[HubCapability],
    params: &ExportSkillsParams,
) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "Cannot determine home directory".to_string())?;
    let tachi_skills_dir = if let Some(ref dir) = params.output_dir {
        PathBuf::from(dir)
    } else {
        home.join(".tachi").join("skills")
    };
    let claude_skills_dir = home.join(".claude").join("skills");

    std::fs::create_dir_all(&tachi_skills_dir)
        .map_err(|e| format!("create {:?}: {e}", tachi_skills_dir))?;
    std::fs::create_dir_all(&claude_skills_dir)
        .map_err(|e| format!("create {:?}: {e}", claude_skills_dir))?;

    let mut exported = Vec::new();
    let mut errors = Vec::new();

    for cap in skills {
        let name = skill_name_from_id(&cap.id);
        let content = match skill_content(cap) {
            Some(c) => c,
            None => {
                errors.push(json!({ "id": cap.id, "error": "no content in definition" }));
                continue;
            }
        };

        let skill_dir = tachi_skills_dir.join(name);
        if let Err(e) = std::fs::create_dir_all(&skill_dir) {
            errors.push(json!({ "id": cap.id, "error": format!("mkdir: {e}") }));
            continue;
        }

        let skill_file = skill_dir.join("SKILL.md");
        if let Err(e) = std::fs::write(&skill_file, &content) {
            errors.push(json!({ "id": cap.id, "error": format!("write: {e}") }));
            continue;
        }

        // Create/update symlink in ~/.claude/skills/
        let link_target = claude_skills_dir.join(name);
        // Remove stale symlink or directory if it exists
        let _ = std::fs::remove_file(&link_target);
        let _ = std::fs::remove_dir(&link_target);
        #[cfg(unix)]
        {
            if let Err(e) = std::os::unix::fs::symlink(&skill_dir, &link_target) {
                errors.push(json!({
                    "id": cap.id,
                    "warning": format!("symlink: {e}"),
                    "file": skill_file.display().to_string()
                }));
            }
        }

        exported.push(json!({
            "id": cap.id,
            "name": name,
            "file": skill_file.display().to_string(),
            "symlink": link_target.display().to_string()
        }));
    }

    // Clean stale skills if requested
    if params.clean {
        let exported_names: HashSet<&str> = skills.iter().map(|c| skill_name_from_id(&c.id)).collect();
        if let Ok(entries) = std::fs::read_dir(&claude_skills_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                if !exported_names.contains(name.as_ref()) {
                    let _ = std::fs::remove_file(entry.path());
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    serde_json::to_string(&json!({
        "agent": "claude",
        "exported": exported.len(),
        "skills_dir": tachi_skills_dir.display().to_string(),
        "claude_skills_dir": claude_skills_dir.display().to_string(),
        "skills": exported,
        "errors": errors
    }))
    .map_err(|e| format!("serialize: {e}"))
}

/// Export skills for OpenClaw: generate a tachi-skills.json plugin config.
fn export_for_openclaw(
    skills: &[HubCapability],
    params: &ExportSkillsParams,
) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "Cannot determine home directory".to_string())?;
    let output_dir = if let Some(ref dir) = params.output_dir {
        PathBuf::from(dir)
    } else {
        home.join(".openclaw").join("plugins")
    };

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("create {:?}: {e}", output_dir))?;

    // Build OpenClaw plugin manifest with skill hooks
    let skill_entries: Vec<serde_json::Value> = skills
        .iter()
        .filter_map(|cap| {
            let name = skill_name_from_id(&cap.id);
            let def: serde_json::Value = serde_json::from_str(&cap.definition).ok()?;
            let description = if cap.description.is_empty() {
                name.to_string()
            } else {
                cap.description.clone()
            };
            Some(json!({
                "id": cap.id,
                "name": name,
                "description": description,
                "prompt": def.get("prompt").or(def.get("template")).cloned(),
                "system": def.get("system").cloned(),
                "model": def.get("model").cloned(),
                "temperature": def.get("temperature").cloned(),
                "trigger": format!("@tachi-skill-{}", name)
            }))
        })
        .collect();

    let manifest = json!({
        "plugin": "tachi-skills",
        "version": "1.0.0",
        "description": "Tachi Hub skills exported for OpenClaw",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "hooks": {
            "before_agent_start": {
                "type": "skill-injection",
                "skills": skill_entries
            }
        },
        "skills": skill_entries
    });

    let manifest_path = output_dir.join("tachi-skills.json");
    let content =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    std::fs::write(&manifest_path, &content)
        .map_err(|e| format!("write {:?}: {e}", manifest_path))?;

    serde_json::to_string(&json!({
        "agent": "openclaw",
        "exported": skills.len(),
        "manifest": manifest_path.display().to_string(),
        "skills": skill_entries.iter().map(|s| s["name"].clone()).collect::<Vec<_>>()
    }))
    .map_err(|e| format!("serialize: {e}"))
}

/// Export skills for Cursor: generate .mdc rule files in .cursor/rules/.
fn export_for_cursor(
    skills: &[HubCapability],
    params: &ExportSkillsParams,
) -> Result<String, String> {
    let output_dir = if let Some(ref dir) = params.output_dir {
        PathBuf::from(dir)
    } else {
        PathBuf::from(".cursor").join("rules")
    };

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("create {:?}: {e}", output_dir))?;

    let mut exported = Vec::new();
    let mut errors = Vec::new();

    for cap in skills {
        let name = skill_name_from_id(&cap.id);
        let content = match skill_content(cap) {
            Some(c) => c,
            None => {
                // Fall back to prompt template
                let def: serde_json::Value =
                    serde_json::from_str(&cap.definition).unwrap_or(json!({}));
                match def
                    .get("prompt")
                    .or(def.get("template"))
                    .and_then(|v| v.as_str())
                {
                    Some(p) => p.to_string(),
                    None => {
                        errors.push(json!({ "id": cap.id, "error": "no content or prompt" }));
                        continue;
                    }
                }
            }
        };

        // Cursor .mdc format with frontmatter
        let mdc_content = format!(
            "---\ndescription: {}\nalwaysApply: false\n---\n\n# {}\n\n{}",
            cap.description.replace('\n', " "),
            cap.name,
            content
        );

        let file_path = output_dir.join(format!("tachi-{}.mdc", name));
        if let Err(e) = std::fs::write(&file_path, &mdc_content) {
            errors.push(json!({ "id": cap.id, "error": format!("write: {e}") }));
            continue;
        }

        exported.push(json!({
            "id": cap.id,
            "name": name,
            "file": file_path.display().to_string()
        }));
    }

    // Clean stale cursor rules if requested
    if params.clean {
        let exported_names: HashSet<&str> = skills.iter().map(|c| skill_name_from_id(&c.id)).collect();
        if let Ok(entries) = std::fs::read_dir(&output_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let name_str = fname.to_string_lossy();
                if name_str.starts_with("tachi-") && name_str.ends_with(".mdc") {
                    let skill_name = name_str
                        .strip_prefix("tachi-")
                        .and_then(|s| s.strip_suffix(".mdc"))
                        .unwrap_or("");
                    if !exported_names.contains(skill_name) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    serde_json::to_string(&json!({
        "agent": "cursor",
        "exported": exported.len(),
        "output_dir": output_dir.display().to_string(),
        "skills": exported,
        "errors": errors
    }))
    .map_err(|e| format!("serialize: {e}"))
}

/// Export skills as raw SKILL.md files to a generic directory.
fn export_for_generic(
    skills: &[HubCapability],
    params: &ExportSkillsParams,
) -> Result<String, String> {
    let output_dir = if let Some(ref dir) = params.output_dir {
        PathBuf::from(dir)
    } else {
        PathBuf::from("exported-skills")
    };

    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("create {:?}: {e}", output_dir))?;

    let mut exported = Vec::new();
    let mut errors = Vec::new();

    for cap in skills {
        let name = skill_name_from_id(&cap.id);
        let content = match skill_content(cap) {
            Some(c) => c,
            None => {
                errors.push(json!({ "id": cap.id, "error": "no content in definition" }));
                continue;
            }
        };

        let file_path = output_dir.join(format!("{}.md", name));
        if let Err(e) = std::fs::write(&file_path, &content) {
            errors.push(json!({ "id": cap.id, "error": format!("write: {e}") }));
            continue;
        }

        exported.push(json!({
            "id": cap.id,
            "name": name,
            "file": file_path.display().to_string()
        }));
    }

    serde_json::to_string(&json!({
        "agent": "generic",
        "exported": exported.len(),
        "output_dir": output_dir.display().to_string(),
        "skills": exported,
        "errors": errors
    }))
    .map_err(|e| format!("serialize: {e}"))
}

// ─── Skill Evolution ─────────────────────────────────────────────────────────

/// Evolve a skill by analyzing its telemetry and using LLM to produce an improved prompt.
///
/// Process:
/// 1. Retrieve the current skill from Hub
/// 2. Gather telemetry (uses, successes, failures, avg_rating)
/// 3. Construct an evolution prompt with the current skill definition + feedback
/// 4. Call LLM to generate an improved prompt
/// 5. Create a new versioned capability and optionally activate it
pub(super) async fn handle_skill_evolve(
    server: &MemoryServer,
    params: SkillEvolveParams,
) -> Result<String, String> {
    // ── 1. Retrieve current skill ────────────────────────────────────────────
    let cap = server
        .get_capability(&params.skill_id)
        .map_err(|e| format!("Skill lookup failed: {e}"))?;

    if cap.cap_type != "skill" {
        return Err(format!(
            "'{}' is type '{}', not 'skill'",
            params.skill_id, cap.cap_type
        ));
    }

    let def: serde_json::Value = serde_json::from_str(&cap.definition)
        .map_err(|e| format!("invalid skill definition JSON: {e}"))?;

    let current_prompt = def
        .get("prompt")
        .or(def.get("template"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let _current_system = def.get("system").and_then(|v| v.as_str()).unwrap_or("");
    let current_content = def.get("content").and_then(|v| v.as_str()).unwrap_or("");

    // ── 2. Build telemetry summary ───────────────────────────────────────────
    let telemetry = format!(
        "Uses: {}, Successes: {}, Failures: {}, Avg Rating: {:.1}/5.0, Health: {}, Fail Streak: {}",
        cap.uses, cap.successes, cap.failures, cap.avg_rating, cap.health_status, cap.fail_streak
    );

    // ── 3. Construct the evolution prompt ────────────────────────────────────
    let user_feedback = params.feedback.as_deref().unwrap_or("No specific feedback provided.");

    let evolution_prompt = format!(
        r#"You are a skill prompt engineer. Your task is to improve the following skill prompt template.

## Current Skill
- **Name:** {name}
- **Description:** {description}
- **Version:** {version}
- **Telemetry:** {telemetry}

## Current Prompt Template
```
{current_prompt}
```

{content_section}

## User Feedback
{user_feedback}

## Instructions
1. Analyze the current prompt template for weaknesses (vagueness, missing constraints, poor structure).
2. Consider the telemetry data — a low success rate or low rating indicates the prompt needs significant improvement.
3. Produce an **improved** prompt template that:
   - Preserves all existing `{{{{placeholder}}}}` variables
   - Is more specific and structured
   - Adds guardrails against common failure modes
   - Improves output quality and consistency
4. Also produce an improved description (1-2 sentences).

## Output Format
Respond with ONLY a JSON object (no markdown fences):
{{
  "prompt": "<improved prompt template>",
  "description": "<improved description>",
  "system": "<improved system prompt, or empty string to keep current>",
  "reasoning": "<brief explanation of what was changed and why>"
}}"#,
        name = cap.name,
        description = cap.description,
        version = cap.version,
        telemetry = telemetry,
        current_prompt = current_prompt,
        content_section = if !current_content.is_empty() {
            format!("## Current SKILL.md Content\n```\n{}\n```", current_content)
        } else {
            String::new()
        },
        user_feedback = user_feedback,
    );

    // ── 4. Call LLM for evolution ────────────────────────────────────────────
    let llm_response = server
        .llm
        .call_llm(
            "You are a skill prompt optimization engine. Output valid JSON only.",
            &evolution_prompt,
            None,
            0.4,
            4000,
        )
        .await
        .map_err(|e| format!("LLM evolution call failed: {e}"))?;

    // Parse LLM response as JSON
    let evolved: serde_json::Value = serde_json::from_str(llm_response.trim())
        .or_else(|_| {
            // Try extracting JSON from markdown code fences
            let trimmed = llm_response.trim();
            let json_str = if let Some(start) = trimmed.find('{') {
                if let Some(end) = trimmed.rfind('}') {
                    &trimmed[start..=end]
                } else {
                    trimmed
                }
            } else {
                trimmed
            };
            serde_json::from_str(json_str)
        })
        .map_err(|e| format!("Failed to parse LLM evolution response as JSON: {e}\nRaw: {llm_response}"))?;

    let new_prompt = evolved
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "LLM response missing 'prompt' field".to_string())?;
    let new_description = evolved
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or(&cap.description);
    let new_system = evolved
        .get("system")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let reasoning = evolved
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("No reasoning provided");

    // ── 5. Dry run — just return the proposal ────────────────────────────────
    if params.dry_run {
        return serde_json::to_string(&json!({
            "skill_id": params.skill_id,
            "dry_run": true,
            "current_version": cap.version,
            "proposed_prompt": new_prompt,
            "proposed_description": new_description,
            "proposed_system": new_system,
            "reasoning": reasoning
        }))
        .map_err(|e| format!("serialize: {e}"));
    }

    // ── 6. Create new versioned capability ───────────────────────────────────
    let new_version = cap.version + 1;
    let new_id = format!("{}/v{}", params.skill_id, new_version);

    // Build new definition by merging evolved fields into current definition
    let mut new_def = def.clone();
    if let Some(obj) = new_def.as_object_mut() {
        obj.insert("prompt".to_string(), json!(new_prompt));
        if let Some(sys) = new_system {
            obj.insert("system".to_string(), json!(sys));
        }
        obj.insert(
            "evolution".to_string(),
            json!({
                "evolved_from": params.skill_id,
                "evolved_from_version": cap.version,
                "reasoning": reasoning,
                "evolved_at": Utc::now().to_rfc3339(),
            }),
        );
    }

    let new_cap = HubCapability {
        id: new_id.clone(),
        cap_type: "skill".to_string(),
        name: cap.name.clone(),
        version: new_version,
        description: new_description.to_string(),
        definition: serde_json::to_string(&new_def).map_err(|e| format!("serialize def: {e}"))?,
        enabled: true,
        review_status: "approved".to_string(),
        health_status: "healthy".to_string(),
        last_error: None,
        last_success_at: None,
        last_failure_at: None,
        fail_streak: 0,
        active_version: None,
        exposure_mode: cap.exposure_mode.clone(),
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
    };

    // Store the new version in the same scope as the original
    server.with_global_store(|store| {
        store
            .hub_register(&new_cap)
            .map_err(|e| format!("persist evolved skill: {e}"))
    })?;

    // ── 7. Optionally activate the new version ───────────────────────────────
    if params.auto_activate {
        server.with_global_store(|store| {
            store
                .hub_set_active_version_route(&params.skill_id, &new_id)
                .map_err(|e| format!("set version route: {e}"))
        })?;

        // Also update the original skill's active_version pointer
        let _ = server.with_global_store(|store| {
            // Read, modify, write back
            if let Some(mut orig) = store.hub_get(&params.skill_id).map_err(|e| format!("{e}"))? {
                orig.active_version = Some(new_id.clone());
                store.hub_register(&orig).map_err(|e| format!("{e}"))?;
            }
            Ok::<(), String>(())
        });
    }

    serde_json::to_string(&json!({
        "skill_id": params.skill_id,
        "evolved_id": new_id,
        "new_version": new_version,
        "auto_activated": params.auto_activate,
        "description": new_description,
        "reasoning": reasoning,
        "prompt_preview": if new_prompt.len() > 200 { &new_prompt[..200] } else { new_prompt }
    }))
    .map_err(|e| format!("serialize: {e}"))
}
