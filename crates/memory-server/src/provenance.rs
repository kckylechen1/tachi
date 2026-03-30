use super::*;

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn current_agent_profile(server: &MemoryServer) -> Option<serde_json::Value> {
    let guard = server
        .agent_profile
        .read()
        .unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|profile| {
        json!({
            "agent_id": profile.agent_id,
            "display_name": profile.display_name,
            "capabilities": profile.capabilities,
            "registered_at": profile.registered_at,
        })
    })
}

fn current_db_path(server: &MemoryServer, target_db: DbScope) -> Option<String> {
    match target_db {
        DbScope::Global => Some(server.global_db_path.display().to_string()),
        DbScope::Project => {
            if let Some(path) = server.project_db_path.as_ref() {
                return Some(path.display().to_string());
            }
            let guard = server
                .hot_project_db
                .read()
                .unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(|state| state.db_path.display().to_string())
        }
    }
}

pub(super) fn inject_provenance(
    server: &MemoryServer,
    metadata: serde_json::Value,
    tool_name: &str,
    source_kind: &str,
    requested_scope: Option<&str>,
    target_db: DbScope,
    extra_context: serde_json::Value,
) -> serde_json::Value {
    let mut metadata_obj = match metadata {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("legacy_metadata".into(), other);
            map
        }
    };

    let mut provenance = serde_json::Map::new();
    provenance.insert("captured_at".into(), json!(Utc::now().to_rfc3339()));
    provenance.insert("tool_name".into(), json!(tool_name));
    provenance.insert("source_kind".into(), json!(source_kind));
    provenance.insert("db_scope".into(), json!(target_db.as_str()));

    if let Some(scope) = requested_scope
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
    {
        provenance.insert("requested_scope".into(), json!(scope));
    }

    if let Some(db_path) = current_db_path(server, target_db) {
        provenance.insert("db_path".into(), json!(db_path));
    }

    if let Some(agent) = current_agent_profile(server) {
        provenance.insert("agent".into(), agent);
    }

    for (field, env_key) in [
        ("profile", "TACHI_PROFILE"),
        ("domain", "TACHI_DOMAIN"),
        ("workspace_id", "TACHI_WORKSPACE_ID"),
        ("project_id", "TACHI_PROJECT_ID"),
        ("session_id", "TACHI_SESSION_ID"),
    ] {
        if let Some(value) = non_empty_env(env_key) {
            provenance.insert(field.into(), json!(value));
        }
    }

    if let serde_json::Value::Object(context) = extra_context {
        if !context.is_empty() {
            provenance.insert("context".into(), serde_json::Value::Object(context));
        }
    }

    metadata_obj.insert("provenance".into(), serde_json::Value::Object(provenance));
    serde_json::Value::Object(metadata_obj)
}
