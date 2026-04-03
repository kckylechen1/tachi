use super::*;

pub(crate) async fn handle_run_skill(
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
        .call_reasoning_llm(
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
    let _ = server.record_capability_call_outcome(&params.skill_id, success, error_msg.as_deref());

    result.map_err(|e| format!("skill execution failed: {}", e))
}

pub(crate) async fn handle_tachi_audit_log(
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

pub(crate) async fn handle_hub_call(
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

pub(crate) async fn handle_hub_disconnect(
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
