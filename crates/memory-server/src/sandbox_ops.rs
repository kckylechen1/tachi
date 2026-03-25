use super::*;

pub(super) async fn handle_sandbox_set_rule(
    server: &MemoryServer,
    params: SandboxSetRuleParams,
) -> Result<String, String> {
    if !["read", "write", "deny"].contains(&params.access_level.as_str()) {
        return Err(format!(
            "Invalid access_level '{}'. Must be: read, write, deny",
            params.access_level
        ));
    }

    server.with_global_store(|store| {
        memory_core::db::set_sandbox_rule(
            store.connection(),
            &params.agent_role,
            &params.path_pattern,
            &params.access_level,
        )
        .map_err(|e| format!("Failed to set sandbox rule: {e}"))
    })?;

    serde_json::to_string(&json!({
        "status": "ok",
        "agent_role": params.agent_role,
        "path_pattern": params.path_pattern,
        "access_level": params.access_level,
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_sandbox_check(
    server: &MemoryServer,
    params: SandboxCheckParams,
) -> Result<String, String> {
    if !["read", "write"].contains(&params.operation.as_str()) {
        return Err(format!(
            "Invalid operation '{}'. Must be: read, write",
            params.operation
        ));
    }

    let (allowed, matching_rule) = server.with_global_store(|store| {
        memory_core::db::check_sandbox_access(
            store.connection(),
            &params.agent_role,
            &params.path,
            &params.operation,
        )
        .map_err(|e| format!("Failed to check sandbox access: {e}"))
    })?;

    serde_json::to_string(&json!({
        "agent_role": params.agent_role,
        "path": params.path,
        "operation": params.operation,
        "allowed": allowed,
        "matching_rule": matching_rule,
    }))
    .map_err(|e| format!("serialize: {e}"))
}
