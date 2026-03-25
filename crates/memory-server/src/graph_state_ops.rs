use super::*;

pub(super) async fn handle_add_edge(
    server: &MemoryServer,
    params: AddEdgeParams,
) -> Result<String, String> {
    let requested_scope = params.scope.clone();
    let (target_db, warning) = server.resolve_write_scope(&requested_scope);

    let edge = memory_core::MemoryEdge {
        source_id: params.source_id.clone(),
        target_id: params.target_id.clone(),
        relation: params.relation.clone(),
        weight: params.weight,
        metadata: params.metadata.unwrap_or(json!({})),
        created_at: Utc::now().to_rfc3339(),
        valid_from: String::new(),
        valid_to: None,
    };

    server.with_store_for_scope(target_db, |store| {
        store
            .add_edge(&edge)
            .map_err(|e| format!("Failed to add edge: {}", e))
    })?;

    let mut resp = serde_json::Map::new();
    resp.insert("ok".into(), json!(true));
    resp.insert("db".into(), json!(target_db.as_str()));
    resp.insert("source_id".into(), json!(params.source_id));
    resp.insert("target_id".into(), json!(params.target_id));
    resp.insert("relation".into(), json!(params.relation));
    if let Some(w) = warning {
        resp.insert("warning".into(), json!(w));
    }

    serde_json::to_string(&resp).map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_get_edges(
    server: &MemoryServer,
    params: GetEdgesParams,
) -> Result<String, String> {
    let (target_db, _warning) = server.resolve_write_scope(&params.scope);

    let edges = server.with_store_for_scope(target_db, |store| {
        store
            .get_edges(
                &params.memory_id,
                &params.direction,
                params.relation_filter.as_deref(),
            )
            .map_err(|e| format!("Failed to get edges: {}", e))
    })?;

    let output: Vec<serde_json::Value> = edges
        .iter()
        .map(|e| {
            json!({
                "db": target_db.as_str(),
                "source_id": e.source_id,
                "target_id": e.target_id,
                "relation": e.relation,
                "weight": e.weight,
                "metadata": e.metadata,
                "created_at": e.created_at,
            })
        })
        .collect();

    serde_json::to_string(&output).map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_set_state(
    server: &MemoryServer,
    params: SetStateParams,
) -> Result<String, String> {
    let value_json =
        serde_json::to_string(&params.value).map_err(|e| format!("Failed to serialize value: {}", e))?;

    server.with_global_store(|store| {
        let version = store
            .set_state("mcp", &params.key, &value_json)
            .map_err(|e| format!("Failed to set state: {}", e))?;

        serde_json::to_string(&json!({
            "key": params.key,
            "value": params.value,
            "version": version
        }))
        .map_err(|e| format!("Failed to serialize response: {}", e))
    })
}

pub(super) async fn handle_get_state(
    server: &MemoryServer,
    params: GetStateParams,
) -> Result<String, String> {
    server.with_global_store(|store| match store.get_state_kv("mcp", &params.key) {
        Ok(Some((value, version))) => {
            let parsed_value: serde_json::Value =
                serde_json::from_str(&value).unwrap_or_else(|_| serde_json::json!(value));

            serde_json::to_string(&json!({
                "key": params.key,
                "value": parsed_value,
                "version": version
            }))
            .map_err(|e| format!("Failed to serialize: {}", e))
        }
        Ok(None) => serde_json::to_string(&json!({
            "key": params.key,
            "error": "not found"
        }))
        .map_err(|e| format!("Failed to serialize: {}", e)),
        Err(e) => Err(format!("Failed to get state: {}", e)),
    })
}
