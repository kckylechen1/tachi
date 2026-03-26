// pack_ops.rs — Pack system server operations
//
// Handles pack registration, listing, removal, and agent projection.

use super::*;
use memory_core::{AgentKind, AgentProjection, Pack};
use std::path::PathBuf;

/// List installed packs.
pub(super) async fn handle_pack_list(
    server: &MemoryServer,
    params: PackListParams,
) -> Result<String, String> {
    let enabled_only = params.enabled_only.unwrap_or(false);
    let packs = server.with_global_store_read(|store| {
        store
            .pack_list(enabled_only)
            .map_err(|e| format!("pack_list: {e}"))
    })?;

    if packs.is_empty() {
        return Ok(r#"{"packs":[],"count":0}"#.to_string());
    }

    serde_json::to_string(&serde_json::json!({
        "packs": packs,
        "count": packs.len(),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

/// Get details of a single pack.
pub(super) async fn handle_pack_get(
    server: &MemoryServer,
    params: PackGetParams,
) -> Result<String, String> {
    let pack = server.with_global_store_read(|store| {
        store
            .pack_get(&params.id)
            .map_err(|e| format!("pack_get: {e}"))
    })?;

    match pack {
        Some(p) => serde_json::to_string(&p).map_err(|e| format!("serialize: {e}")),
        None => Err(format!("Pack '{}' not found", params.id)),
    }
}

/// Register a pack (used after git clone / download).
pub(super) async fn handle_pack_register(
    server: &MemoryServer,
    params: PackRegisterParams,
) -> Result<String, String> {
    let local_path = params.local_path.clone().unwrap_or_default();

    // Count skills in the pack directory
    let skill_count = if !local_path.is_empty() {
        count_skills_in_dir(&local_path)
    } else {
        0
    };

    let pack = Pack {
        id: params.id.clone(),
        name: params.name.unwrap_or_else(|| params.id.clone()),
        source: params.source.unwrap_or_default(),
        version: params.version.unwrap_or_else(|| "latest".to_string()),
        description: params.description.unwrap_or_default(),
        skill_count,
        enabled: true,
        local_path,
        metadata: params
            .metadata
            .map(|v| serde_json::to_string(&v).unwrap_or_default())
            .unwrap_or_else(|| "{}".to_string()),
        installed_at: String::new(),
        updated_at: String::new(),
    };

    server.with_global_store(|store| {
        store
            .pack_register(&pack)
            .map_err(|e| format!("pack_register: {e}"))
    })?;

    Ok(serde_json::json!({
        "status": "registered",
        "pack_id": params.id,
        "skill_count": skill_count,
    })
    .to_string())
}

/// Remove a pack and its projections.
pub(super) async fn handle_pack_remove(
    server: &MemoryServer,
    params: PackRemoveParams,
) -> Result<String, String> {
    // Get pack info first to know projected paths
    let pack = server.with_global_store_read(|store| {
        store
            .pack_get(&params.id)
            .map_err(|e| format!("pack_get: {e}"))
    })?;

    if pack.is_none() {
        return Err(format!("Pack '{}' not found", params.id));
    }

    // Get all projections for this pack
    let projections = server.with_global_store_read(|store| {
        store
            .projection_list(None, Some(&params.id))
            .map_err(|e| format!("projection_list: {e}"))
    })?;

    // Clean up projected files if requested
    let mut cleaned_agents = Vec::new();
    if params.clean_files.unwrap_or(true) {
        for proj in &projections {
            if !proj.projected_path.is_empty() {
                let path = PathBuf::from(&proj.projected_path);
                if path.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        tracing::warn!("Failed to remove projected files at {}: {e}", proj.projected_path);
                    } else {
                        cleaned_agents.push(proj.agent.clone());
                    }
                }
            }
        }
    }

    // Delete from DB
    let deleted = server.with_global_store(|store| {
        store
            .pack_delete(&params.id)
            .map_err(|e| format!("pack_delete: {e}"))
    })?;

    Ok(serde_json::json!({
        "status": if deleted { "removed" } else { "not_found" },
        "pack_id": params.id,
        "cleaned_agents": cleaned_agents,
    })
    .to_string())
}

/// Project a pack's skills to one or more agents.
pub(super) async fn handle_pack_project(
    server: &MemoryServer,
    params: PackProjectParams,
) -> Result<String, String> {
    let pack = server
        .with_global_store_read(|store| {
            store
                .pack_get(&params.pack_id)
                .map_err(|e| format!("pack_get: {e}"))
        })?
        .ok_or_else(|| format!("Pack '{}' not found", params.pack_id))?;

    let agents: Vec<AgentKind> = params
        .agents
        .iter()
        .filter_map(|s| AgentKind::from_str(s))
        .collect();

    if agents.is_empty() {
        return Err("No valid agent kinds provided. Use: claude, codex, cursor, gemini, opencode, antigravity, trae, kiro".to_string());
    }

    let mut results = Vec::new();

    for agent in &agents {
        match project_pack_to_agent(&pack, *agent) {
            Ok((projected_path, count)) => {
                let proj = AgentProjection {
                    agent: agent.as_str().to_string(),
                    pack_id: pack.id.clone(),
                    enabled: true,
                    projected_path: projected_path.clone(),
                    skill_count: count,
                    synced_at: String::new(),
                };

                if let Err(e) = server.with_global_store(|store| {
                    store
                        .projection_upsert(&proj)
                        .map_err(|e| format!("projection_upsert: {e}"))
                }) {
                    tracing::warn!("Failed to save projection record for {}: {e}", agent.as_str());
                }

                results.push(serde_json::json!({
                    "agent": agent.as_str(),
                    "status": "projected",
                    "path": projected_path,
                    "skill_count": count,
                }));
            }
            Err(e) => {
                results.push(serde_json::json!({
                    "agent": agent.as_str(),
                    "status": "failed",
                    "error": e,
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "pack_id": pack.id,
        "projections": results,
    })
    .to_string())
}

/// List agent projections.
pub(super) async fn handle_projection_list(
    server: &MemoryServer,
    params: ProjectionListParams,
) -> Result<String, String> {
    let projections = server.with_global_store_read(|store| {
        store
            .projection_list(params.agent.as_deref(), params.pack_id.as_deref())
            .map_err(|e| format!("projection_list: {e}"))
    })?;

    serde_json::to_string(&serde_json::json!({
        "projections": projections,
        "count": projections.len(),
    }))
    .map_err(|e| format!("serialize: {e}"))
}

// ─── Projection Logic ────────────────────────────────────────────────────────

/// Project a pack's skills into an agent's native format.
/// Returns (projected_path, skill_count).
fn project_pack_to_agent(pack: &Pack, agent: AgentKind) -> Result<(String, u32), String> {
    let source_dir = PathBuf::from(&pack.local_path);
    if !source_dir.exists() {
        return Err(format!(
            "Pack source directory not found: {}",
            pack.local_path
        ));
    }

    let (base_dir_template, _file_ext) = agent.skill_target();
    let home = dirs::home_dir().ok_or_else(|| "Cannot determine home directory".to_string())?;
    let base_dir = base_dir_template.replace("~", &home.to_string_lossy());
    let pack_short_name = pack.id.split('/').last().unwrap_or(&pack.id);
    let target_dir = PathBuf::from(&base_dir).join(pack_short_name);

    // Create target directory
    std::fs::create_dir_all(&target_dir)
        .map_err(|e| format!("Failed to create {}: {e}", target_dir.display()))?;

    let count;

    match agent {
        AgentKind::Claude | AgentKind::Antigravity | AgentKind::Kiro => {
            // Claude-family: Copy entire pack directory structure
            // Skills are SKILL.md files in subdirectories
            count = copy_skill_tree(&source_dir, &target_dir)?;
        }
        AgentKind::Codex => {
            // Codex: Uses .agents/skills/ or ~/.codex/skills/ with SKILL.md
            // gstack already generates Codex-format skills in .agents/skills/
            let codex_skills = source_dir.join(".agents").join("skills");
            if codex_skills.exists() {
                count = copy_skill_tree(&codex_skills, &target_dir)?;
            } else {
                // Fallback: copy root SKILL.md files
                count = copy_skill_tree(&source_dir, &target_dir)?;
            }
        }
        AgentKind::Cursor => {
            // Cursor: Uses .mdc rule files in ~/.cursor/rules/
            count = project_skills_as_cursor_rules(&source_dir, &target_dir, pack_short_name)?;
        }
        AgentKind::Gemini | AgentKind::OpenCode | AgentKind::Trae | AgentKind::Generic => {
            // Generic: Copy SKILL.md files into target directory
            count = copy_skill_tree(&source_dir, &target_dir)?;
        }
    }

    Ok((target_dir.to_string_lossy().to_string(), count))
}

/// Count SKILL.md files in a directory tree.
fn count_skills_in_dir(dir: &str) -> u32 {
    let path = PathBuf::from(dir);
    if !path.exists() {
        return 0;
    }
    let mut count = 0u32;
    if let Ok(entries) = std::fs::read_dir(&path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let skill_file = p.join("SKILL.md");
                if skill_file.exists() {
                    count += 1;
                }
            }
        }
    }
    // Check root SKILL.md
    if path.join("SKILL.md").exists() {
        count += 1;
    }
    count
}

/// Copy a skill directory tree, preserving structure.
/// Returns number of SKILL.md files copied.
fn copy_skill_tree(source: &PathBuf, target: &PathBuf) -> Result<u32, String> {
    let mut count = 0u32;

    // Copy root SKILL.md if exists
    let root_skill = source.join("SKILL.md");
    if root_skill.exists() {
        let dest = target.join("SKILL.md");
        std::fs::copy(&root_skill, &dest)
            .map_err(|e| format!("copy SKILL.md: {e}"))?;
        count += 1;
    }

    // Copy subdirectory SKILL.md files
    let entries = std::fs::read_dir(source)
        .map_err(|e| format!("read_dir {}: {e}", source.display()))?;

    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let skill_file = p.join("SKILL.md");
            if skill_file.exists() {
                let dir_name = p
                    .file_name()
                    .ok_or_else(|| "no file name".to_string())?
                    .to_string_lossy()
                    .to_string();
                // Skip hidden directories and non-skill directories
                if dir_name.starts_with('.') || dir_name == "node_modules" || dir_name == "browse" || dir_name == "scripts" || dir_name == "test" || dir_name == "benchmark" || dir_name == "docs" || dir_name == "lib" || dir_name == "bin" {
                    continue;
                }
                let sub_target = target.join(&dir_name);
                std::fs::create_dir_all(&sub_target)
                    .map_err(|e| format!("mkdir {}: {e}", sub_target.display()))?;
                std::fs::copy(&skill_file, sub_target.join("SKILL.md"))
                    .map_err(|e| format!("copy {}/SKILL.md: {e}", dir_name))?;
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Convert SKILL.md files to Cursor .mdc rule format.
fn project_skills_as_cursor_rules(
    source: &PathBuf,
    target: &PathBuf,
    pack_name: &str,
) -> Result<u32, String> {
    let mut count = 0u32;

    // Convert root SKILL.md
    let root_skill = source.join("SKILL.md");
    if root_skill.exists() {
        let content = std::fs::read_to_string(&root_skill)
            .map_err(|e| format!("read SKILL.md: {e}"))?;
        let mdc = format_as_mdc(pack_name, "main", &content);
        let dest = target.join(format!("{pack_name}.mdc"));
        std::fs::write(&dest, mdc).map_err(|e| format!("write mdc: {e}"))?;
        count += 1;
    }

    // Convert subdirectory SKILL.md files
    let entries = std::fs::read_dir(source)
        .map_err(|e| format!("read_dir: {e}"))?;

    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let skill_file = p.join("SKILL.md");
            if skill_file.exists() {
                let dir_name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if dir_name.starts_with('.') || dir_name == "node_modules" {
                    continue;
                }
                let content = std::fs::read_to_string(&skill_file)
                    .map_err(|e| format!("read {}/SKILL.md: {e}", dir_name))?;
                let mdc = format_as_mdc(pack_name, &dir_name, &content);
                let dest = target.join(format!("{pack_name}-{dir_name}.mdc"));
                std::fs::write(&dest, mdc).map_err(|e| format!("write mdc: {e}"))?;
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Format a SKILL.md content as a Cursor .mdc rule.
fn format_as_mdc(pack_name: &str, skill_name: &str, content: &str) -> String {
    format!(
        r#"---
description: {pack_name}/{skill_name} skill
globs: 
alwaysApply: false
---

{content}
"#
    )
}
