use super::*;

/// Evolve a skill by analyzing its telemetry and using LLM to produce an improved prompt.
///
/// Process:
/// 1. Retrieve the current skill from Hub
/// 2. Gather telemetry (uses, successes, failures, avg_rating)
/// 3. Construct an evolution prompt with the current skill definition + feedback
/// 4. Call LLM to generate an improved prompt
/// 5. Create a new versioned capability and optionally activate it
pub(crate) async fn handle_skill_evolve(
    server: &MemoryServer,
    params: SkillEvolveParams,
) -> Result<String, String> {
    // ── 1. Retrieve current skill ────────────────────────────────────────────
    let cap = server
        .get_capability(&params.skill_id)
        .map_err(|e| format!("Skill lookup failed: {e}"))?;

    if cap.cap_type != "skill" {
        return Err(format!(
            "'{}' is type '{}', not 'skill'",
            params.skill_id, cap.cap_type
        ));
    }

    let def: serde_json::Value = serde_json::from_str(&cap.definition)
        .map_err(|e| format!("invalid skill definition JSON: {e}"))?;

    let current_prompt = def
        .get("prompt")
        .or(def.get("template"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let _current_system = def.get("system").and_then(|v| v.as_str()).unwrap_or("");
    let current_content = def.get("content").and_then(|v| v.as_str()).unwrap_or("");

    // ── 2. Build telemetry summary ───────────────────────────────────────────
    let telemetry = format!(
        "Uses: {}, Successes: {}, Failures: {}, Avg Rating: {:.1}/5.0, Health: {}, Fail Streak: {}",
        cap.uses, cap.successes, cap.failures, cap.avg_rating, cap.health_status, cap.fail_streak
    );

    // ── 3. Construct the evolution prompt ────────────────────────────────────
    let user_feedback = params
        .feedback
        .as_deref()
        .unwrap_or("No specific feedback provided.");

    let evolution_prompt = format!(
        r#"You are a skill prompt engineer. Your task is to improve the following skill prompt template.

## Current Skill
- **Name:** {name}
- **Description:** {description}
- **Version:** {version}
- **Telemetry:** {telemetry}

## Current Prompt Template
```
{current_prompt}
```

{content_section}

## User Feedback
{user_feedback}

## Instructions
1. Analyze the current prompt template for weaknesses (vagueness, missing constraints, poor structure).
2. Consider the telemetry data — a low success rate or low rating indicates the prompt needs significant improvement.
3. Produce an **improved** prompt template that:
   - Preserves all existing `{{{{placeholder}}}}` variables
   - Is more specific and structured
   - Adds guardrails against common failure modes
   - Improves output quality and consistency
4. Also produce an improved description (1-2 sentences).

## Output Format
Respond with ONLY a JSON object (no markdown fences):
{{
  "prompt": "<improved prompt template>",
  "description": "<improved description>",
  "system": "<improved system prompt, or empty string to keep current>",
  "reasoning": "<brief explanation of what was changed and why>"
}}"#,
        name = cap.name,
        description = cap.description,
        version = cap.version,
        telemetry = telemetry,
        current_prompt = current_prompt,
        content_section = if !current_content.is_empty() {
            format!("## Current SKILL.md Content\n```\n{}\n```", current_content)
        } else {
            String::new()
        },
        user_feedback = user_feedback,
    );

    // ── 4. Call LLM for evolution ────────────────────────────────────────────
    let llm_response = server
        .llm
        .call_reasoning_llm(
            "You are a skill prompt optimization engine. Output valid JSON only.",
            &evolution_prompt,
            None,
            0.4,
            4000,
        )
        .await
        .map_err(|e| format!("LLM evolution call failed: {e}"))?;

    // Parse LLM response as JSON
    let evolved: serde_json::Value = serde_json::from_str(llm_response.trim())
        .or_else(|_| {
            // Try extracting JSON from markdown code fences
            let trimmed = llm_response.trim();
            let json_str = if let Some(start) = trimmed.find('{') {
                if let Some(end) = trimmed.rfind('}') {
                    &trimmed[start..=end]
                } else {
                    trimmed
                }
            } else {
                trimmed
            };
            serde_json::from_str(json_str)
        })
        .map_err(|e| {
            format!("Failed to parse LLM evolution response as JSON: {e}\nRaw: {llm_response}")
        })?;

    let new_prompt = evolved
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "LLM response missing 'prompt' field".to_string())?;
    let new_description = evolved
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or(&cap.description);
    let new_system = evolved
        .get("system")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let reasoning = evolved
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("No reasoning provided");

    // ── 5. Dry run — just return the proposal ────────────────────────────────
    if params.dry_run {
        return serde_json::to_string(&json!({
            "skill_id": params.skill_id,
            "dry_run": true,
            "current_version": cap.version,
            "proposed_prompt": new_prompt,
            "proposed_description": new_description,
            "proposed_system": new_system,
            "reasoning": reasoning
        }))
        .map_err(|e| format!("serialize: {e}"));
    }

    // ── 6. Create new versioned capability ───────────────────────────────────
    let new_version = cap.version + 1;
    let new_id = format!("{}/v{}", params.skill_id, new_version);

    // Build new definition by merging evolved fields into current definition
    let mut new_def = def.clone();
    if let Some(obj) = new_def.as_object_mut() {
        obj.insert("prompt".to_string(), json!(new_prompt));
        if let Some(sys) = new_system {
            obj.insert("system".to_string(), json!(sys));
        }
        obj.insert(
            "evolution".to_string(),
            json!({
                "evolved_from": params.skill_id,
                "evolved_from_version": cap.version,
                "reasoning": reasoning,
                "evolved_at": Utc::now().to_rfc3339(),
            }),
        );
    }

    let new_cap = HubCapability {
        id: new_id.clone(),
        cap_type: "skill".to_string(),
        name: cap.name.clone(),
        version: new_version,
        description: new_description.to_string(),
        definition: serde_json::to_string(&new_def).map_err(|e| format!("serialize def: {e}"))?,
        enabled: true,
        review_status: "approved".to_string(),
        health_status: "healthy".to_string(),
        last_error: None,
        last_success_at: None,
        last_failure_at: None,
        fail_streak: 0,
        active_version: None,
        exposure_mode: cap.exposure_mode.clone(),
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
    };

    // Store the new version in the same scope as the original
    server.with_global_store(|store| {
        store
            .hub_register(&new_cap)
            .map_err(|e| format!("persist evolved skill: {e}"))
    })?;

    // ── 7. Optionally activate the new version ───────────────────────────────
    if params.auto_activate {
        server.with_global_store(|store| {
            store
                .hub_set_active_version_route(&params.skill_id, &new_id)
                .map_err(|e| format!("set version route: {e}"))
        })?;

        // Also update the original skill's active_version pointer
        let _ = server.with_global_store(|store| {
            // Read, modify, write back
            if let Some(mut orig) = store
                .hub_get(&params.skill_id)
                .map_err(|e| format!("{e}"))?
            {
                orig.active_version = Some(new_id.clone());
                store.hub_register(&orig).map_err(|e| format!("{e}"))?;
            }
            Ok::<(), String>(())
        });
    }

    serde_json::to_string(&json!({
        "skill_id": params.skill_id,
        "evolved_id": new_id,
        "new_version": new_version,
        "auto_activated": params.auto_activate,
        "description": new_description,
        "reasoning": reasoning,
        "prompt_preview": if new_prompt.len() > 200 {
            &new_prompt[..new_prompt.char_indices().nth(200).map(|(i, _)| i).unwrap_or(new_prompt.len())]
        } else {
            new_prompt
        }
    }))
    .map_err(|e| format!("serialize: {e}"))
}
