use super::*;

const AGENT_EVOLUTION_PROPOSAL_SOURCE: &str = "foundry_agent_evolution";
const FOUNDRY_JOB_NAMESPACE: &str = "foundry_job";
const FOUNDRY_PROPOSAL_REVIEW_NAMESPACE: &str = "foundry_proposal_review";

fn parse_document_kind(raw: &str) -> Result<memory_core::AgentProfileDocumentKind, String> {
    memory_core::AgentProfileDocumentKind::parse(raw).ok_or_else(|| {
        format!(
            "Unknown document kind '{}'. Expected identity|agents|latest_truths|routing_policy|tool_policy|memory_policy|other",
            raw
        )
    })
}

fn parse_evidence_kind(raw: &str) -> Result<memory_core::FoundryEvidenceKind, String> {
    memory_core::FoundryEvidenceKind::parse(raw).ok_or_else(|| {
        format!(
            "Unknown evidence kind '{}'. Expected memory|reflection|tooluse|eval|ghost|session_outcome|skill_telemetry|profile_snapshot|proposal|other",
            raw
        )
    })
}

fn build_foundry_job(
    server: &MemoryServer,
    params: &SynthesizeAgentEvolutionParams,
) -> memory_core::FoundryJobSpec {
    let requested_by = read_or_recover(&server.agent_profile, "agent_profile")
        .as_ref()
        .map(|profile| profile.agent_id.clone());

    memory_core::FoundryJobSpec {
        id: format!("foundry-job:{}", uuid::Uuid::new_v4()),
        kind: memory_core::FoundryJobKind::AgentEvolution,
        lane: memory_core::FoundryModelLane::Reasoning,
        status: memory_core::FoundryJobStatus::Planned,
        target_agent_id: Some(params.agent_id.clone()),
        requested_by,
        created_at: Utc::now().to_rfc3339(),
        evidence_count: params.evidence.len(),
        goal_count: params.goals.len(),
        metadata: json!({
            "display_name": params.display_name,
            "document_count": params.documents.len(),
        }),
    }
}

fn build_documents(
    params: &SynthesizeAgentEvolutionParams,
) -> Result<Vec<memory_core::AgentProfileDocument>, String> {
    params
        .documents
        .iter()
        .map(|doc| {
            Ok(memory_core::AgentProfileDocument {
                kind: parse_document_kind(&doc.kind)?,
                path: doc.path.clone(),
                content: doc.content.clone(),
            })
        })
        .collect()
}

fn build_evidence(
    params: &SynthesizeAgentEvolutionParams,
) -> Result<Vec<memory_core::FoundryEvidence>, String> {
    params
        .evidence
        .iter()
        .map(|item| {
            Ok(memory_core::FoundryEvidence {
                kind: parse_evidence_kind(&item.kind)?,
                title: item.title.clone(),
                content: item.content.clone(),
                source_ref: item.source_ref.clone(),
                path: item.path.clone(),
                weight: item.weight.max(0.0),
            })
        })
        .collect()
}

fn build_synthesis_payload(
    params: &SynthesizeAgentEvolutionParams,
    documents: &[memory_core::AgentProfileDocument],
    evidence: &[memory_core::FoundryEvidence],
) -> Value {
    json!({
        "agent": {
            "agent_id": params.agent_id,
            "display_name": params.display_name,
        },
        "goals": params.goals,
        "documents": documents,
        "evidence": evidence,
    })
}

fn parse_synthesis_response(raw: &str) -> Result<memory_core::AgentEvolutionSynthesis, String> {
    let json_str = llm::LlmClient::strip_code_fence(raw);
    serde_json::from_str(json_str).map_err(|e| {
        format!(
            "Failed to parse agent evolution synthesis JSON: {e} — response was: {}",
            json_str
        )
    })
}

fn proposal_root(agent_id: &str) -> String {
    format!(
        "/foundry/agents/{}/proposals",
        sanitize_safe_path_name(agent_id)
    )
}

fn review_status_or_default(raw: Option<&str>) -> String {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "approved" => "approved".to_string(),
        "rejected" => "rejected".to_string(),
        "applied" => "applied".to_string(),
        _ => "proposed".to_string(),
    }
}

fn parse_review_status(raw: &str) -> Result<String, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "approved" => Ok("approved".to_string()),
        "rejected" => Ok("rejected".to_string()),
        "applied" => Ok("applied".to_string()),
        other => Err(format!(
            "Invalid review status '{}'. Expected approved|rejected|applied",
            other
        )),
    }
}

fn save_foundry_job_state(
    server: &MemoryServer,
    job: &memory_core::FoundryJobSpec,
    status: &str,
    extra: serde_json::Value,
) -> Result<(), String> {
    let mut payload = serde_json::Map::new();
    payload.insert("job".into(), json!(job));
    payload.insert("status".into(), json!(status));
    payload.insert("updated_at".into(), json!(Utc::now().to_rfc3339()));
    if let Some(extra_obj) = extra.as_object() {
        for (key, value) in extra_obj {
            payload.insert(key.clone(), value.clone());
        }
    }
    let value_json = serde_json::to_string(&serde_json::Value::Object(payload))
        .map_err(|e| format!("Failed to serialize foundry job state: {e}"))?;
    server.with_global_store(|store| {
        store
            .set_state(FOUNDRY_JOB_NAMESPACE, &job.id, &value_json)
            .map_err(|e| format!("Failed to persist foundry job state: {e}"))?;
        Ok(())
    })
}

fn load_review_state(
    server: &MemoryServer,
    proposal_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    server.with_global_store(|store| {
        match store.get_state_kv(FOUNDRY_PROPOSAL_REVIEW_NAMESPACE, proposal_id) {
            Ok(Some((value, _version))) => {
                let parsed = serde_json::from_str(&value)
                    .unwrap_or_else(|_| json!({ "raw": value }));
                Ok(Some(parsed))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(format!("Failed to load proposal review state: {e}")),
        }
    })
}

fn resolve_foundry_write_scope(server: &MemoryServer) -> (DbScope, Option<String>) {
    server.resolve_write_scope("project")
}

fn persist_agent_evolution_proposal(
    server: &MemoryServer,
    params: &SynthesizeAgentEvolutionParams,
    job: &memory_core::FoundryJobSpec,
    synthesis: &memory_core::AgentEvolutionSynthesis,
) -> Result<(String, String, DbScope), String> {
    let root = proposal_root(&params.agent_id);
    let path = format!(
        "{}/{}-{}",
        root,
        Utc::now().format("%Y%m%dT%H%M%S"),
        sanitize_safe_path_name(&job.id)
    );
    let summary = if synthesis.summary.trim().is_empty() {
        synthesis
            .no_change_reason
            .clone()
            .unwrap_or_else(|| "agent evolution proposal".to_string())
    } else {
        synthesis.summary.clone()
    };
    let text = serde_json::to_string_pretty(synthesis)
        .map_err(|e| format!("Failed to serialize agent evolution synthesis: {e}"))?;
    let (target_db, _warning) = resolve_foundry_write_scope(server);
    let metadata = crate::provenance::inject_provenance(
        server,
        json!({
            "job": job,
            "agent_id": params.agent_id,
            "display_name": params.display_name,
            "goals": params.goals,
            "document_count": params.documents.len(),
            "evidence_count": params.evidence.len(),
            "proposal_count": synthesis.proposals.len(),
            "status": "proposed",
        }),
        "synthesize_agent_evolution",
        "agent_evolution_proposal",
        Some(target_db.as_str()),
        target_db,
        json!({
            "agent_id": params.agent_id,
            "proposal_path": path,
        }),
    );
    let derived_id = server.with_store_for_scope(target_db, |store| {
        store
            .save_derived(
                &text,
                &path,
                &summary,
                0.8,
                AGENT_EVOLUTION_PROPOSAL_SOURCE,
                target_db.as_str(),
                &metadata,
            )
            .map_err(|e| format!("Failed to save agent evolution proposal: {e}"))
    })?;
    Ok((derived_id, path, target_db))
}

async fn run_agent_evolution_synthesis(
    server: &MemoryServer,
    params: &SynthesizeAgentEvolutionParams,
    job: &memory_core::FoundryJobSpec,
) -> Result<(memory_core::AgentEvolutionSynthesis, String, String, DbScope), String> {
    let documents = build_documents(params)?;
    let evidence = build_evidence(params)?;
    let payload = build_synthesis_payload(params, &documents, &evidence);
    let user = serde_json::to_string_pretty(&payload)
        .map_err(|e| format!("Failed to serialize synthesis request: {e}"))?;
    let response = server
        .llm
        .call_llm(
            crate::prompts::AGENT_EVOLUTION_SYNTHESIS_PROMPT,
            &user,
            None,
            0.2,
            2400,
        )
        .await?;
    let synthesis = parse_synthesis_response(&response)?;
    let (proposal_id, proposal_path, target_db) =
        persist_agent_evolution_proposal(server, params, job, &synthesis)?;
    Ok((synthesis, proposal_id, proposal_path, target_db))
}

fn parse_derived_metadata(row: &serde_json::Value) -> serde_json::Value {
    row.get("metadata")
        .and_then(|value| value.as_str())
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| json!({}))
}

fn parse_derived_synthesis(row: &serde_json::Value) -> Option<memory_core::AgentEvolutionSynthesis> {
    row.get("text")
        .and_then(|value| value.as_str())
        .and_then(|raw| serde_json::from_str(raw).ok())
}

fn proposal_record_from_row(
    row: &serde_json::Value,
    review: Option<serde_json::Value>,
) -> serde_json::Value {
    let metadata = parse_derived_metadata(row);
    let status = review_status_or_default(
        review
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(|value| value.as_str())
            .or_else(|| metadata.get("status").and_then(|value| value.as_str())),
    );
    json!({
        "proposal_id": row.get("id").and_then(|value| value.as_str()).unwrap_or(""),
        "path": row.get("path").and_then(|value| value.as_str()).unwrap_or(""),
        "summary": row.get("summary").and_then(|value| value.as_str()).unwrap_or(""),
        "created_at": row.get("created_at").and_then(|value| value.as_str()).unwrap_or(""),
        "status": status,
        "metadata": metadata,
        "review": review,
        "synthesis": parse_derived_synthesis(row),
    })
}

fn load_agent_proposal_rows(
    server: &MemoryServer,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let root = proposal_root(agent_id);
    let (target_db, _warning) = resolve_foundry_write_scope(server);
    server.with_store_for_scope_read(target_db, |store| {
        store
            .list_derived_by_source(AGENT_EVOLUTION_PROPOSAL_SOURCE, &root, limit)
            .map_err(|e| format!("Failed to list agent evolution proposals: {e}"))
    })
}

fn markdown_heading_title(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let title = trimmed.trim_start_matches('#').trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

fn apply_markdown_section_update(
    content: &str,
    section: Option<&str>,
    suggested_value: &str,
) -> String {
    let replacement = suggested_value.trim();
    if replacement.is_empty() {
        return content.to_string();
    }

    let Some(section) = section.map(str::trim).filter(|section| !section.is_empty()) else {
        if content.trim().is_empty() {
            return format!("{replacement}\n");
        }
        return format!("{}\n\n{}\n", content.trim_end(), replacement);
    };

    let lines = content.lines().collect::<Vec<_>>();
    let mut heading_index = None;
    for (idx, line) in lines.iter().enumerate() {
        if markdown_heading_title(line)
            .map(|title| title == section)
            .unwrap_or(false)
        {
            heading_index = Some(idx);
            break;
        }
    }

    if let Some(start_idx) = heading_index {
        let mut end_idx = lines.len();
        for idx in (start_idx + 1)..lines.len() {
            if markdown_heading_title(lines[idx]).is_some() {
                end_idx = idx;
                break;
            }
        }

        let mut rebuilt = Vec::new();
        rebuilt.extend_from_slice(&lines[..=start_idx]);
        rebuilt.push("");
        rebuilt.extend(replacement.lines());
        if end_idx < lines.len() {
            rebuilt.push("");
            rebuilt.extend_from_slice(&lines[end_idx..]);
        }
        return rebuilt.join("\n").trim_end().to_string() + "\n";
    }

    let mut suffix = String::new();
    if !content.trim().is_empty() {
        suffix.push_str(content.trim_end());
        suffix.push_str("\n\n");
    }
    suffix.push_str(&format!("## {section}\n\n{replacement}\n"));
    suffix
}

fn document_target_aliases(doc: &AgentEvolutionDocumentParams) -> Vec<String> {
    let mut aliases = Vec::new();
    if let Some(path) = &doc.path {
        if let Some(name) = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
        {
            aliases.push(name.to_ascii_lowercase());
        }
    }
    let kind_alias = match parse_document_kind(&doc.kind) {
        Ok(memory_core::AgentProfileDocumentKind::Identity) => Some("identity.md"),
        Ok(memory_core::AgentProfileDocumentKind::Agents) => Some("agents.md"),
        Ok(memory_core::AgentProfileDocumentKind::LatestTruths) => Some("latest_truths.md"),
        Ok(memory_core::AgentProfileDocumentKind::RoutingPolicy) => Some("routing_policy.md"),
        Ok(memory_core::AgentProfileDocumentKind::ToolPolicy) => Some("tool_policy.md"),
        Ok(memory_core::AgentProfileDocumentKind::MemoryPolicy) => Some("memory_policy.md"),
        _ => None,
    };
    if let Some(alias) = kind_alias {
        aliases.push(alias.to_string());
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

fn proposal_targets_document(
    proposal: &memory_core::AgentEvolutionProposal,
    doc: &AgentEvolutionDocumentParams,
) -> bool {
    let target = proposal.target.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }
    document_target_aliases(doc)
        .into_iter()
        .any(|alias| alias == target)
}

fn write_document_if_requested(path: &str, content: &str) -> Result<(), String> {
    std::fs::write(path, content)
        .map_err(|e| format!("Failed to write projected document {path}: {e}"))
}

fn mark_proposal_applied(server: &MemoryServer, proposal_id: &str) -> Result<(), String> {
    let reviewer = read_or_recover(&server.agent_profile, "agent_profile")
        .as_ref()
        .map(|profile| profile.agent_id.clone());
    let mut review = load_review_state(server, proposal_id)?.unwrap_or_else(|| json!({}));
    if let Some(obj) = review.as_object_mut() {
        obj.insert("status".into(), json!("applied"));
        obj.insert("applied_at".into(), json!(Utc::now().to_rfc3339()));
        obj.insert("applied_by".into(), json!(reviewer));
    }
    let value_json = serde_json::to_string(&review)
        .map_err(|e| format!("Failed to serialize applied proposal state: {e}"))?;
    server.with_global_store(|store| {
        store
            .set_state(FOUNDRY_PROPOSAL_REVIEW_NAMESPACE, proposal_id, &value_json)
            .map_err(|e| format!("Failed to persist applied proposal state: {e}"))?;
        Ok(())
    })
}

pub(super) async fn handle_synthesize_agent_evolution(
    server: &MemoryServer,
    params: SynthesizeAgentEvolutionParams,
) -> Result<String, String> {
    if params.documents.is_empty() && params.evidence.is_empty() {
        return Err(
            "synthesize_agent_evolution requires at least one document or evidence item"
                .to_string(),
        );
    }

    let documents = build_documents(&params)?;
    let evidence = build_evidence(&params)?;
    let job = build_foundry_job(server, &params);
    let payload = build_synthesis_payload(&params, &documents, &evidence);

    if params.dry_run {
        return serde_json::to_string(&json!({
            "status": "dry_run",
            "job": job,
            "request": payload,
        }))
        .map_err(|e| format!("Failed to serialize dry-run response: {e}"));
    }

    let (synthesis, proposal_id, proposal_path, target_db) =
        run_agent_evolution_synthesis(server, &params, &job).await?;

    serde_json::to_string(&json!({
        "status": "completed",
        "job": job,
        "proposal_id": proposal_id,
        "proposal_path": proposal_path,
        "db": target_db.as_str(),
        "synthesis": synthesis,
    }))
    .map_err(|e| format!("Failed to serialize synthesis response: {e}"))
}

pub(super) async fn handle_queue_agent_evolution(
    server: &MemoryServer,
    params: SynthesizeAgentEvolutionParams,
) -> Result<String, String> {
    if params.documents.is_empty() && params.evidence.is_empty() {
        return Err(
            "queue_agent_evolution requires at least one document or evidence item".to_string(),
        );
    }

    let mut job = build_foundry_job(server, &params);
    job.status = memory_core::FoundryJobStatus::Queued;
    save_foundry_job_state(server, &job, "queued", json!({}))?;

    let server = server.clone();
    let params_for_task = params.clone();
    let job_for_task = job.clone();
    tokio::spawn(async move {
        if let Err(err) = save_foundry_job_state(&server, &job_for_task, "running", json!({})) {
            eprintln!(
                "[foundry-agent-evolution] failed to update job {} to running: {err}",
                job_for_task.id
            );
        }

        match run_agent_evolution_synthesis(&server, &params_for_task, &job_for_task).await {
            Ok((synthesis, proposal_id, proposal_path, target_db)) => {
                let _ = save_foundry_job_state(
                    &server,
                    &job_for_task,
                    "completed",
                    json!({
                        "proposal_id": proposal_id,
                        "proposal_path": proposal_path,
                        "db": target_db.as_str(),
                        "proposal_count": synthesis.proposals.len(),
                    }),
                );
            }
            Err(err) => {
                eprintln!(
                    "[foundry-agent-evolution] job {} failed: {err}",
                    job_for_task.id
                );
                let _ = save_foundry_job_state(
                    &server,
                    &job_for_task,
                    "failed",
                    json!({ "error": err }),
                );
            }
        }
    });

    serde_json::to_string(&json!({
        "status": "queued",
        "job": job,
    }))
    .map_err(|e| format!("Failed to serialize queue response: {e}"))
}

pub(super) async fn handle_list_agent_evolution_proposals(
    server: &MemoryServer,
    params: ListAgentEvolutionProposalsParams,
) -> Result<String, String> {
    let root = proposal_root(&params.agent_id);
    let limit = params.limit.max(1).min(100);
    let (target_db, _warning) = resolve_foundry_write_scope(server);
    let rows = server.with_store_for_scope_read(target_db, |store| {
        store
            .list_derived_by_source(AGENT_EVOLUTION_PROPOSAL_SOURCE, &root, limit)
            .map_err(|e| format!("Failed to list agent evolution proposals: {e}"))
    })?;

    let desired_status = params
        .status
        .as_deref()
        .map(|status| review_status_or_default(Some(status)));
    let mut proposals = Vec::new();
    for row in rows {
        let proposal_id = row
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let review = load_review_state(server, &proposal_id)?;
        let record = proposal_record_from_row(&row, review);
        let status = record
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("proposed");
        if let Some(ref desired) = desired_status {
            if status != desired {
                continue;
            }
        }
        proposals.push(record);
    }

    serde_json::to_string(&json!({
        "agent_id": params.agent_id,
        "count": proposals.len(),
        "proposals": proposals,
    }))
    .map_err(|e| format!("Failed to serialize proposal list: {e}"))
}

pub(super) async fn handle_review_agent_evolution_proposal(
    server: &MemoryServer,
    params: ReviewAgentEvolutionProposalParams,
) -> Result<String, String> {
    let status = parse_review_status(&params.status)?;
    let reviewer = read_or_recover(&server.agent_profile, "agent_profile")
        .as_ref()
        .map(|profile| profile.agent_id.clone());
    let review = json!({
        "status": status,
        "note": params.note,
        "reviewed_at": Utc::now().to_rfc3339(),
        "reviewed_by": reviewer,
    });
    let value_json = serde_json::to_string(&review)
        .map_err(|e| format!("Failed to serialize proposal review: {e}"))?;
    server.with_global_store(|store| {
        store
            .set_state(FOUNDRY_PROPOSAL_REVIEW_NAMESPACE, &params.proposal_id, &value_json)
            .map_err(|e| format!("Failed to persist proposal review: {e}"))?;
        Ok(())
    })?;

    serde_json::to_string(&json!({
        "proposal_id": params.proposal_id,
        "review": review,
    }))
    .map_err(|e| format!("Failed to serialize proposal review response: {e}"))
}

pub(super) async fn handle_project_agent_profile(
    server: &MemoryServer,
    params: ProjectAgentProfileParams,
) -> Result<String, String> {
    if params.documents.is_empty() {
        return Err("project_agent_profile requires at least one document".to_string());
    }

    let proposal_id_filter = params
        .proposal_ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect::<std::collections::HashSet<_>>();
    let rows = load_agent_proposal_rows(server, &params.agent_id, 100)?;

    let mut proposal_records = Vec::<serde_json::Value>::new();
    for row in rows {
        let proposal_id = row
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if !proposal_id_filter.is_empty() && !proposal_id_filter.contains(&proposal_id) {
            continue;
        }
        let review = load_review_state(server, &proposal_id)?;
        let record = proposal_record_from_row(&row, review);
        let status = record
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("proposed");
        if params.approved_only && status != "approved" {
            continue;
        }
        proposal_records.push(record);
    }

    if proposal_records.is_empty() {
        return serde_json::to_string(&json!({
            "status": "skipped",
            "reason": "no_matching_proposals",
            "agent_id": params.agent_id,
        }))
        .map_err(|e| format!("Failed to serialize projection response: {e}"));
    }

    let mut projected_documents = Vec::<serde_json::Value>::new();
    let mut applied_proposal_ids = std::collections::HashSet::<String>::new();

    for doc in &params.documents {
        let mut content = doc.content.clone();
        let mut applied = Vec::<serde_json::Value>::new();
        for record in &proposal_records {
            let proposal_id = record
                .get("proposal_id")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let status = record
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("proposed")
                .to_string();
            let Some(synthesis) = record.get("synthesis").cloned().and_then(|value| {
                serde_json::from_value::<memory_core::AgentEvolutionSynthesis>(value).ok()
            }) else {
                continue;
            };
            for proposal in synthesis.proposals {
                if !proposal_targets_document(&proposal, doc) {
                    continue;
                }
                content = apply_markdown_section_update(
                    &content,
                    proposal.target_section.as_deref(),
                    &proposal.suggested_value,
                );
                applied.push(json!({
                    "proposal_id": proposal_id,
                    "status": status,
                    "title": proposal.title,
                    "target_section": proposal.target_section,
                }));
                applied_proposal_ids.insert(proposal_id.clone());
            }
        }

        let written = if params.write && !applied.is_empty() {
            let path = doc.path.as_deref().ok_or_else(|| {
                "project_agent_profile write=true requires document paths".to_string()
            })?;
            write_document_if_requested(path, &content)?;
            true
        } else {
            false
        };

        projected_documents.push(json!({
            "kind": doc.kind,
            "path": doc.path,
            "written": written,
            "applied_proposals": applied,
            "content": content,
        }));
    }

    if params.write {
        for proposal_id in &applied_proposal_ids {
            mark_proposal_applied(server, proposal_id)?;
        }
    }

    serde_json::to_string(&json!({
        "status": "completed",
        "agent_id": params.agent_id,
        "applied_count": applied_proposal_ids.len(),
        "documents": projected_documents,
    }))
    .map_err(|e| format!("Failed to serialize projection response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_synthesis_response_accepts_json_object() {
        let parsed = parse_synthesis_response(
            r#"{
              "summary":"stable overall",
              "stable_signals":["keeps verifying changes"],
              "drift_signals":["routing is inconsistent"],
              "proposals":[
                {
                  "title":"Tighten routing policy",
                  "target":"AGENTS.md",
                  "target_section":"Runtime 分流（硬规则）",
                  "current_value":"old",
                  "suggested_value":"new",
                  "rationale":"eval shows repeated mismatch",
                  "risk":"medium",
                  "evidence_refs":["eval:2026-04-01"]
                }
              ],
              "no_change_reason":null
            }"#,
        )
        .expect("response should parse");

        assert_eq!(parsed.proposals.len(), 1);
        assert_eq!(parsed.proposals[0].target, "AGENTS.md");
    }

    #[test]
    fn parse_review_status_accepts_expected_values() {
        assert_eq!(parse_review_status("approved").unwrap(), "approved");
        assert_eq!(parse_review_status("rejected").unwrap(), "rejected");
        assert_eq!(parse_review_status("applied").unwrap(), "applied");
        assert!(parse_review_status("queued").is_err());
    }

    #[test]
    fn apply_markdown_section_update_replaces_existing_section() {
        let original = "# Identity\n\n## Routing\n\nold value\n\n## Other\n\nstay\n";
        let updated = apply_markdown_section_update(original, Some("Routing"), "new value");
        assert!(updated.contains("## Routing\n\nnew value"));
        assert!(updated.contains("## Other\n\nstay"));
        assert!(!updated.contains("old value"));
    }

    #[test]
    fn apply_markdown_section_update_appends_missing_section() {
        let updated =
            apply_markdown_section_update("# Identity\n", Some("Memory Policy"), "write less");
        assert!(updated.contains("## Memory Policy"));
        assert!(updated.contains("write less"));
    }
}
