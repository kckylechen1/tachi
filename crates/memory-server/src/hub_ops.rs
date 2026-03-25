use super::*;

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
            match server.discover_mcp_tools(&def).await {
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
