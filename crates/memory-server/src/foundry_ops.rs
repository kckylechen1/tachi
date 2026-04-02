use super::*;

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

    serde_json::to_string(&json!({
        "status": "completed",
        "job": job,
        "synthesis": synthesis,
    }))
    .map_err(|e| format!("Failed to serialize synthesis response: {e}"))
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
}
