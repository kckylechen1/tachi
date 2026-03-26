use super::*;

pub(super) async fn handle_get_memory(
    server: &MemoryServer,
    params: GetMemoryParams,
) -> Result<String, String> {
    if server.project_db_path.is_some() {
        let project_entry = server.with_project_store_read(|store| {
            store
                .get_with_options(&params.id, params.include_archived)
                .map_err(|e| format!("Failed to get memory from project DB: {}", e))
        })?;

        if let Some(entry) = project_entry {
            return serde_json::to_string(&slim_entry(&entry, DbScope::Project))
                .map_err(|e| format!("Failed to serialize: {}", e));
        }
    }

    let global_entry = server.with_global_store_read(|store| {
        store
            .get_with_options(&params.id, params.include_archived)
            .map_err(|e| format!("Failed to get memory from global DB: {}", e))
    })?;

    match global_entry {
        Some(entry) => serde_json::to_string(&slim_entry(&entry, DbScope::Global))
            .map_err(|e| format!("Failed to serialize: {}", e)),
        None => serde_json::to_string(&json!({
            "error": "Memory not found"
        }))
        .map_err(|e| format!("Failed to serialize: {}", e)),
    }
}

pub(super) async fn handle_list_memories(
    server: &MemoryServer,
    params: ListMemoriesParams,
) -> Result<String, String> {
    let mut combined_entries: Vec<(MemoryEntry, DbScope)> = Vec::new();

    let global_entries = server.with_global_store_read(|store| {
        store
            .list_by_path(&params.path_prefix, params.limit, params.include_archived)
            .map_err(|e| format!("Failed to list memories from global DB: {}", e))
    })?;
    combined_entries.extend(global_entries.into_iter().map(|e| (e, DbScope::Global)));

    if server.project_db_path.is_some() {
        let project_entries = server.with_project_store_read(|store| {
            store
                .list_by_path(&params.path_prefix, params.limit, params.include_archived)
                .map_err(|e| format!("Failed to list memories from project DB: {}", e))
        })?;
        combined_entries.extend(project_entries.into_iter().map(|e| (e, DbScope::Project)));
    }

    combined_entries.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));
    combined_entries.truncate(params.limit);

    let slim: Vec<serde_json::Value> = combined_entries
        .iter()
        .map(|(e, db_scope)| slim_entry(e, *db_scope))
        .collect();
    serde_json::to_string(&slim).map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_memory_stats(server: &MemoryServer) -> Result<String, String> {
    let global_stats = server.with_global_store_read(|store| {
        store
            .stats(false)
            .map_err(|e| format!("Failed to get global stats: {}", e))
    })?;

    let project_stats = if server.project_db_path.is_some() {
        Some(server.with_project_store_read(|store| {
            store
                .stats(false)
                .map_err(|e| format!("Failed to get project stats: {}", e))
        })?)
    } else {
        None
    };

    let mut total = global_stats.total;
    let mut by_scope: HashMap<String, u64> = global_stats.by_scope.clone();
    let mut by_category: HashMap<String, u64> = global_stats.by_category.clone();
    let mut by_root_path: HashMap<String, u64> = global_stats.by_root_path.clone();

    if let Some(ref project_stats) = project_stats {
        total += project_stats.total;

        for (k, v) in &project_stats.by_scope {
            *by_scope.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &project_stats.by_category {
            *by_category.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &project_stats.by_root_path {
            *by_root_path.entry(k.clone()).or_insert(0) += v;
        }
    }

    let mut databases = serde_json::Map::new();
    databases.insert(
        "global".into(),
        json!({
            "path": server.global_db_path.display().to_string(),
            "vec_available": server.global_vec_available,
            "total": global_stats.total,
            "by_scope": global_stats.by_scope,
            "by_category": global_stats.by_category,
        }),
    );
    if let Some(ref ps) = project_stats {
        databases.insert(
            "project".into(),
            json!({
                "path": server.project_db_path.as_ref().map(|p| p.display().to_string()),
                "vec_available": server.project_vec_available,
                "total": ps.total,
                "by_scope": ps.by_scope,
                "by_category": ps.by_category,
            }),
        );
    }

    serde_json::to_string(&json!({
        "total": total,
        "by_scope": by_scope,
        "by_category": by_category,
        "by_root_path": by_root_path,
        "databases": databases,
    }))
    .map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_delete_memory(
    server: &MemoryServer,
    params: DeleteMemoryParams,
) -> Result<String, String> {
    if server.project_db_path.is_some() {
        let deleted = server.with_project_store(|store| {
            store
                .delete(&params.id)
                .map_err(|e| format!("Delete failed: {}", e))
        })?;
        if deleted {
            return serde_json::to_string(
                &json!({ "deleted": true, "db": "project", "id": params.id }),
            )
            .map_err(|e| format!("Failed to serialize: {}", e));
        }
    }

    let deleted = server.with_global_store(|store| {
        store
            .delete(&params.id)
            .map_err(|e| format!("Delete failed: {}", e))
    })?;

    serde_json::to_string(&json!({
        "deleted": deleted,
        "db": if deleted { "global" } else { "not_found" },
        "id": params.id,
    }))
    .map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_archive_memory(
    server: &MemoryServer,
    params: ArchiveMemoryParams,
) -> Result<String, String> {
    if server.project_db_path.is_some() {
        let archived = server.with_project_store(|store| {
            store
                .archive_memory(&params.id)
                .map_err(|e| format!("Archive failed: {}", e))
        })?;
        if archived {
            return serde_json::to_string(
                &json!({ "archived": true, "db": "project", "id": params.id }),
            )
            .map_err(|e| format!("Failed to serialize: {}", e));
        }
    }

    let archived = server.with_global_store(|store| {
        store
            .archive_memory(&params.id)
            .map_err(|e| format!("Archive failed: {}", e))
    })?;

    serde_json::to_string(&json!({
        "archived": archived,
        "db": if archived { "global" } else { "not_found" },
        "id": params.id,
    }))
    .map_err(|e| format!("Failed to serialize: {}", e))
}

pub(super) async fn handle_memory_gc(server: &MemoryServer) -> Result<String, String> {
    let mut results = serde_json::Map::new();

    let global_gc = server.with_global_store(|store| {
        store
            .gc_tables()
            .map_err(|e| format!("GC failed on global DB: {}", e))
    })?;
    results.insert("global".into(), global_gc);

    if server.project_db_path.is_some() {
        let project_gc = server.with_project_store(|store| {
            store
                .gc_tables()
                .map_err(|e| format!("GC failed on project DB: {}", e))
        })?;
        results.insert("project".into(), project_gc);
    }

    serde_json::to_string(&results).map_err(|e| format!("Failed to serialize: {}", e))
}
