use super::*;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use memory_core::scorer::local_pagerank;
use std::collections::HashMap;

fn default_checks() -> Vec<String> {
    vec![
        "orphans".to_string(),
        "contradictions".to_string(),
        "stale".to_string(),
        "missing_edges".to_string(),
    ]
}

fn parse_rfc3339_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn tokenize_for_similarity(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn token_cosine_similarity(a: &str, b: &str) -> f64 {
    let mut freq_a: HashMap<String, f64> = HashMap::new();
    let mut freq_b: HashMap<String, f64> = HashMap::new();
    for token in tokenize_for_similarity(a) {
        *freq_a.entry(token).or_insert(0.0) += 1.0;
    }
    for token in tokenize_for_similarity(b) {
        *freq_b.entry(token).or_insert(0.0) += 1.0;
    }
    if freq_a.is_empty() || freq_b.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let norm_a = freq_a.values().map(|v| v * v).sum::<f64>().sqrt();
    let norm_b = freq_b.values().map(|v| v * v).sum::<f64>().sqrt();
    for (token, value_a) in &freq_a {
        if let Some(value_b) = freq_b.get(token) {
            dot += value_a * value_b;
        }
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

fn contradiction_score(a: &str, b: &str) -> f64 {
    let negations = ["never", "not", "avoid", "disable", "forbid", "against", "cannot"];
    let affirmations = ["always", "use", "enable", "allow", "prefer", "should"];
    let a_lower = a.to_ascii_lowercase();
    let b_lower = b.to_ascii_lowercase();
    let a_neg = negations.iter().any(|token| a_lower.contains(token));
    let b_neg = negations.iter().any(|token| b_lower.contains(token));
    let a_aff = affirmations.iter().any(|token| a_lower.contains(token));
    let b_aff = affirmations.iter().any(|token| b_lower.contains(token));
    if (a_neg && b_aff) || (b_neg && a_aff) {
        token_cosine_similarity(a, b)
    } else {
        0.0
    }
}

fn extract_skill_content(cap: &HubCapability) -> Option<String> {
    let def: Value = serde_json::from_str(&cap.definition).ok()?;
    def.get("content")
        .and_then(|v| v.as_str())
        .or_else(|| def.get("prompt").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn extract_skill_path(cap: &HubCapability) -> Option<String> {
    let def: Value = serde_json::from_str(&cap.definition).ok()?;
    def.get("skill_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn set_skill_quality_metadata(def: &mut Value, patch: Value) {
    if !def.is_object() {
        *def = json!({});
    }
    let obj = def.as_object_mut().expect("def should be object");
    let quality = obj
        .entry("quality_guard")
        .or_insert_with(|| json!({}));
    if !quality.is_object() {
        *quality = json!({});
    }
    if let (Some(target), Some(source)) = (quality.as_object_mut(), patch.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn latest_snapshot_for_skill(store: &mut MemoryStore, skill_path: &str) -> Option<MemoryEntry> {
    let root = format!("{}/distilled", skill_path.trim_end_matches('/'));
    store
        .list_by_path(&root, 50, false)
        .ok()?
        .into_iter()
        .max_by(|a, b| a.timestamp.cmp(&b.timestamp))
}

fn relation_exists(edges: &[memory_core::MemoryEdge], a: &str, b: &str, relation: Option<&str>) -> bool {
    edges.iter().any(|edge| {
        let matches_nodes =
            (edge.source_id == a && edge.target_id == b) || (edge.source_id == b && edge.target_id == a);
        let matches_relation = relation.map(|rel| edge.relation == rel).unwrap_or(true);
        matches_nodes && matches_relation
    })
}

#[derive(Clone)]
struct SkillQualitySnapshot {
    cap: HubCapability,
    def: Value,
    content: String,
    latest_snapshot: Option<MemoryEntry>,
}

fn run_skill_quality_guards_for_scope(
    server: &MemoryServer,
    scope: DbScope,
) -> Result<Value, String> {
    let mut snapshots: Vec<SkillQualitySnapshot> = server.with_store_for_scope_read(scope, |store| {
        let caps = store
            .hub_list(Some("skill"), false)
            .map_err(|e| format!("hub list skills: {e}"))?;
        let mut out = Vec::new();
        for cap in caps {
            let Some(content) = extract_skill_content(&cap) else {
                continue;
            };
            let def: Value = serde_json::from_str(&cap.definition).unwrap_or_else(|_| json!({}));
            let skill_path = extract_skill_path(&cap);
            let latest_snapshot = skill_path
                .as_deref()
                .and_then(|path| latest_snapshot_for_skill(store, path));
            out.push(SkillQualitySnapshot {
                cap,
                def,
                content,
                latest_snapshot,
            });
        }
        Ok(out)
    })?;

    let now = Utc::now();
    let mut merge_map: HashMap<String, Vec<Value>> = HashMap::new();
    let mut graph_edges = Vec::<memory_core::MemoryEdge>::new();

    for i in 0..snapshots.len() {
        for j in (i + 1)..snapshots.len() {
            let similarity = token_cosine_similarity(&snapshots[i].content, &snapshots[j].content);
            if similarity > 0.92 {
                merge_map.entry(snapshots[i].cap.id.clone()).or_default().push(json!({
                    "skill_id": snapshots[j].cap.id,
                    "similarity": similarity,
                }));
                merge_map.entry(snapshots[j].cap.id.clone()).or_default().push(json!({
                    "skill_id": snapshots[i].cap.id,
                    "similarity": similarity,
                }));

                if let (Some(left), Some(right)) = (
                    snapshots[i].latest_snapshot.as_ref(),
                    snapshots[j].latest_snapshot.as_ref(),
                ) {
                    graph_edges.push(memory_core::MemoryEdge {
                        source_id: left.id.clone(),
                        target_id: right.id.clone(),
                        relation: "related_to".to_string(),
                        weight: similarity.clamp(0.0, 1.0),
                        metadata: json!({
                            "source": "skill_quality_guard",
                            "type": "merge_hint",
                            "similarity": similarity,
                        }),
                        created_at: now.to_rfc3339(),
                        valid_from: String::new(),
                        valid_to: None,
                    });
                }
            }
        }
    }

    if !graph_edges.is_empty() {
        let edges = graph_edges.clone();
        let _ = server.with_store_for_scope(scope, |store| {
            for edge in &edges {
                store.add_edge(edge).map_err(|e| format!("skill graph edge: {e}"))?;
            }
            Ok(())
        });
    }

    let pagerank = local_pagerank(&graph_edges, 0.85);
    let mut archived_skills = Vec::<String>::new();
    let mut changed_caps = Vec::<HubCapability>::new();

    for snapshot in &mut snapshots {
        let merge_hints = merge_map
            .get(&snapshot.cap.id)
            .cloned()
            .unwrap_or_default();
        let pagerank_score = snapshot
            .latest_snapshot
            .as_ref()
            .and_then(|memory| pagerank.get(&memory.id).copied())
            .unwrap_or(0.0);
        let mut new_def = snapshot.def.clone();
        set_skill_quality_metadata(
            &mut new_def,
            json!({
                "merge_hints": merge_hints,
                "pagerank": pagerank_score,
                "updated_at": now.to_rfc3339(),
            }),
        );

        let stale_cutoff = now - ChronoDuration::days(30);
        let should_archive = snapshot.cap.avg_rating < 0.3
            && snapshot
                .cap
                .last_used
                .as_deref()
                .and_then(parse_rfc3339_utc)
                .map(|ts| ts < stale_cutoff)
                .unwrap_or(false);
        if should_archive {
            if !new_def.is_object() {
                new_def = json!({});
            }
            let obj = new_def.as_object_mut().expect("object");
            let policy = obj.entry("policy").or_insert_with(|| json!({}));
            if !policy.is_object() {
                *policy = json!({});
            }
            if let Some(policy_obj) = policy.as_object_mut() {
                policy_obj.insert("visibility".to_string(), json!("hidden"));
            }
            set_skill_quality_metadata(
                &mut new_def,
                json!({
                    "status": "archived",
                    "archived_reason": "stale_low_rating",
                    "archived_at": now.to_rfc3339(),
                }),
            );
            archived_skills.push(snapshot.cap.id.clone());
        }

        let serialized = serde_json::to_string(&new_def)
            .map_err(|e| format!("serialize skill quality def: {e}"))?;
        if serialized != snapshot.cap.definition {
            let mut updated = snapshot.cap.clone();
            updated.definition = serialized;
            changed_caps.push(updated);
        }
    }

    if !changed_caps.is_empty() {
        let caps_to_store = changed_caps.clone();
        server.with_store_for_scope(scope, |store| {
            for cap in &caps_to_store {
                store
                    .hub_register(cap)
                    .map_err(|e| format!("hub register skill quality update: {e}"))?;
            }
            Ok(())
        })?;

        for cap in &changed_caps {
            if capability_callable(cap) && should_expose_skill_tool(cap) {
                let _ = server.register_skill_tool(cap);
            } else {
                let _ = server.unregister_skill_tool(&cap.id);
            }
        }
    }

    Ok(json!({
        "scope": scope.as_str(),
        "archived_skills": archived_skills,
        "merge_hints": merge_map,
        "pagerank": pagerank,
        "updated_caps": changed_caps.iter().map(|cap| cap.id.clone()).collect::<Vec<_>>(),
    }))
}

pub(crate) fn refresh_skill_quality_guards(server: &MemoryServer) -> Result<Value, String> {
    let global = run_skill_quality_guards_for_scope(server, DbScope::Global)?;
    let project = if server.has_project_db() {
        Some(run_skill_quality_guards_for_scope(server, DbScope::Project)?)
    } else {
        None
    };
    Ok(json!({"global": global, "project": project}))
}

// ─── Wiki Search ────────────────────────────────────────────────────────────

/// Wiki category prefixes for quick lookup. Resolves short names to full paths.
fn resolve_wiki_category(category: &str) -> String {
    let trimmed = category.trim().trim_start_matches('/');
    // Already a full wiki path
    if trimmed.starts_with("wiki/") || trimmed.starts_with("wiki\\") {
        return format!("/{}", trimmed);
    }
    // Short alias → full path
    match trimmed.to_ascii_lowercase().as_str() {
        "quant" | "trading" => "/wiki/quant".to_string(),
        "quant/strategy" | "strategy" => "/wiki/quant/strategy".to_string(),
        "quant/stock-analysis" | "stock-analysis" | "stock" => {
            "/wiki/quant/stock-analysis".to_string()
        }
        "quant/portfolio" | "portfolio" => "/wiki/quant/portfolio".to_string(),
        "quant/market-analysis" | "market-analysis" | "market" => {
            "/wiki/quant/market-analysis".to_string()
        }
        "quant/data-pipeline" | "data-pipeline" | "data" => {
            "/wiki/quant/data-pipeline".to_string()
        }
        "quant/autoresearch" | "autoresearch" => "/wiki/quant/autoresearch".to_string(),
        "engineering" | "eng" | "code" | "coding" => "/wiki/engineering".to_string(),
        "engineering/architecture" | "architecture" | "arch" => {
            "/wiki/engineering/architecture".to_string()
        }
        "engineering/devops" | "devops" => "/wiki/engineering/devops".to_string(),
        "engineering/debugging" | "debugging" | "debug" => {
            "/wiki/engineering/debugging".to_string()
        }
        "engineering/code-review" | "code-review" | "review" => {
            "/wiki/engineering/code-review".to_string()
        }
        "agent" => "/wiki/agent".to_string(),
        "agent/tachi" | "tachi" => "/wiki/agent/tachi".to_string(),
        "agent/openclaw" | "openclaw" => "/wiki/agent/openclaw".to_string(),
        "agent/evolution" | "evolution" => "/wiki/agent/evolution".to_string(),
        "product" => "/wiki/product".to_string(),
        "product/hyperion" | "hyperion" => "/wiki/product/hyperion".to_string(),
        "product/crimson-alphard" | "crimson-alphard" | "crimson" => {
            "/wiki/product/crimson-alphard".to_string()
        }
        "misc" => "/wiki/misc".to_string(),
        other => format!("/wiki/{}", other),
    }
}

/// All known wiki top-level categories for browse stats.
const WIKI_CATEGORIES: &[&str] = &[
    "/wiki/quant/strategy",
    "/wiki/quant/stock-analysis",
    "/wiki/quant/portfolio",
    "/wiki/quant/market-analysis",
    "/wiki/quant/data-pipeline",
    "/wiki/quant/autoresearch",
    "/wiki/engineering/architecture",
    "/wiki/engineering/devops",
    "/wiki/engineering/debugging",
    "/wiki/engineering/code-review",
    "/wiki/agent/tachi",
    "/wiki/agent/openclaw",
    "/wiki/agent/evolution",
    "/wiki/product/hyperion",
    "/wiki/product/crimson-alphard",
    "/wiki/misc",
];

pub(crate) async fn handle_wiki_search(
    server: &MemoryServer,
    params: WikiSearchParams,
) -> Result<String, String> {
    if params.query.trim().is_empty() {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "empty_query",
            "count": 0,
            "results": [],
        }))
        .map_err(|e| format!("serialize wiki_search: {e}"));
    }

    let path_prefix = params
        .category
        .as_deref()
        .map(resolve_wiki_category)
        .or_else(|| Some("/wiki".to_string()));

    let rows = search_memory_rows(
        server,
        SearchMemoryParams {
            query: params.query.clone(),
            query_vec: None,
            top_k: params.top_k.max(1).min(50),
            path_prefix,
            include_archived: false,
            candidates_per_channel: params.top_k.max(20),
            mmr_threshold: Some(0.85),
            graph_expand_hops: 1,
            graph_relation_filter: None,
            weights: params.weights,
            agent_role: None,
            project: Some(params.project),
            domain: None,
        },
    )
    .await?;

    serde_json::to_string(&json!({
        "status": "completed",
        "count": rows.len(),
        "results": rows,
    }))
    .map_err(|e| format!("serialize wiki_search: {e}"))
}

pub(crate) fn handle_wiki_browse(
    server: &MemoryServer,
    params: WikiBrowseParams,
) -> Result<String, String> {
    let project_name = params.project;

    match params.category.as_deref() {
        None | Some("") => {
            // Return category stats (counts per category)
            let mut categories = Vec::new();
            let mut total = 0usize;

            for &cat_path in WIKI_CATEGORIES {
                let count = server
                    .with_named_project_store_read(&project_name, |store| {
                        store
                            .list_by_path(cat_path, 5000, false)
                            .map(|entries| entries.len())
                            .map_err(|e| format!("wiki_browse count: {e}"))
                    })
                    .unwrap_or(0);
                if count > 0 {
                    categories.push(json!({
                        "path": cat_path,
                        "count": count,
                    }));
                    total += count;
                }
            }

            serde_json::to_string(&json!({
                "status": "completed",
                "mode": "stats",
                "total_entries": total,
                "categories": categories,
            }))
            .map_err(|e| format!("serialize wiki_browse: {e}"))
        }
        Some(category) => {
            // Browse a specific category
            let resolved_path = resolve_wiki_category(category);
            let limit = params.limit.max(1).min(500);

            let entries = server.with_named_project_store_read(&project_name, |store| {
                store
                    .list_by_path(&resolved_path, limit, false)
                    .map_err(|e| format!("wiki_browse list: {e}"))
            })?;

            let slim_entries: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    json!({
                        "id": entry.id,
                        "path": entry.path,
                        "summary": entry.summary,
                        "topic": entry.topic,
                        "importance": entry.importance,
                        "keywords": entry.keywords,
                        "timestamp": entry.timestamp,
                    })
                })
                .collect();

            serde_json::to_string(&json!({
                "status": "completed",
                "mode": "browse",
                "path": resolved_path,
                "count": slim_entries.len(),
                "entries": slim_entries,
            }))
            .map_err(|e| format!("serialize wiki_browse: {e}"))
        }
    }
}

// ─── Wiki Lint ──────────────────────────────────────────────────────────────

pub(crate) async fn handle_wiki_lint(
    server: &MemoryServer,
    params: WikiLintParams,
) -> Result<String, String> {
    let checks: Vec<String> = if params.checks.is_empty() {
        default_checks()
    } else {
        params.checks.clone()
    };
    let path_prefix = params.path_prefix.as_deref().unwrap_or("/wiki");
    let limit = params.limit.max(1).min(500);
    let stale_cutoff = Utc::now() - ChronoDuration::days(params.stale_days as i64);

    let mut nodes: Vec<(MemoryEntry, DbScope)> = Vec::new();
    let global_entries = server.with_global_store_read(|store| {
        store
            .list_by_path(path_prefix, limit, false)
            .map_err(|e| format!("wiki_lint global list: {e}"))
    })?;
    nodes.extend(global_entries.into_iter().map(|entry| (entry, DbScope::Global)));
    if server.has_project_db() {
        let project_entries = server.with_project_store_read(|store| {
            store
                .list_by_path(path_prefix, limit, false)
                .map_err(|e| format!("wiki_lint project list: {e}"))
        })?;
        nodes.extend(project_entries.into_iter().map(|entry| (entry, DbScope::Project)));
    }

    let mut orphans = Vec::new();
    let mut stale_nodes = Vec::new();
    let mut contradiction_candidates = Vec::new();
    let mut missing_edge_hints = Vec::new();

    let mut all_edges = Vec::<memory_core::MemoryEdge>::new();
    for (entry, scope) in &nodes {
        let edges = if *scope == DbScope::Global {
            server.with_global_store_read(|store| {
                store
                    .get_edges(&entry.id, "both", None)
                    .map_err(|e| format!("wiki_lint get edges: {e}"))
            })?
        } else {
            server.with_project_store_read(|store| {
                store
                    .get_edges(&entry.id, "both", None)
                    .map_err(|e| format!("wiki_lint get edges: {e}"))
            })?
        };
        if checks.iter().any(|check| check == "orphans") && edges.is_empty() {
            orphans.push(json!({
                "id": entry.id,
                "path": entry.path,
                "db": scope.as_str(),
            }));
        }
        all_edges.extend(edges);
        if checks.iter().any(|check| check == "stale") {
            if let Some(ts) = parse_rfc3339_utc(&entry.timestamp) {
                if ts < stale_cutoff
                    && !matches!(entry.retention_policy.as_deref(), Some("permanent" | "pinned"))
                {
                    stale_nodes.push(json!({
                        "id": entry.id,
                        "path": entry.path,
                        "timestamp": entry.timestamp,
                        "db": scope.as_str(),
                    }));
                }
            }
        }
    }

    if checks
        .iter()
        .any(|check| check == "contradictions" || check == "missing_edges")
    {
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                let left = &nodes[i].0;
                let right = &nodes[j].0;
                if nodes[i].1 != nodes[j].1 {
                    continue;
                }
                let similarity = token_cosine_similarity(&left.text, &right.text);
                if checks.iter().any(|check| check == "missing_edges")
                    && similarity > params.missing_edge_threshold
                    && !relation_exists(&all_edges, &left.id, &right.id, None)
                {
                    missing_edge_hints.push(json!({
                        "left_id": left.id,
                        "right_id": right.id,
                        "left_path": left.path,
                        "right_path": right.path,
                        "similarity": similarity,
                        "db": nodes[i].1.as_str(),
                    }));
                }
                if checks.iter().any(|check| check == "contradictions") {
                    let contradiction = contradiction_score(&left.text, &right.text);
                    if contradiction > params.contradiction_threshold {
                        contradiction_candidates.push(json!({
                            "left_id": left.id,
                            "right_id": right.id,
                            "left_path": left.path,
                            "right_path": right.path,
                            "score": contradiction,
                            "db": nodes[i].1.as_str(),
                        }));
                    }
                }
            }
        }
    }

    let skill_quality = refresh_skill_quality_guards(server)?;

    serde_json::to_string(&json!({
        "path_prefix": path_prefix,
        "checks": checks,
        "orphans": orphans,
        "stale_nodes": stale_nodes,
        "contradiction_candidates": contradiction_candidates,
        "missing_edge_hints": missing_edge_hints,
        "skill_quality": skill_quality,
    }))
    .map_err(|e| format!("serialize wiki_lint: {e}"))
}
