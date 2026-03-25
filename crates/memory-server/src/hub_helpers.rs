use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapabilityVisibility {
    Listed,
    Discoverable,
    Hidden,
}

impl CapabilityVisibility {
    pub(super) fn from_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "listed" | "public" | "show" => Some(Self::Listed),
            "discoverable" | "on_demand" | "on-demand" | "call_only" | "call-only" => {
                Some(Self::Discoverable)
            }
            "hidden" | "private" | "off" => Some(Self::Hidden),
            _ => None,
        }
    }

    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::Discoverable => "discoverable",
            Self::Hidden => "hidden",
        }
    }
}

pub(super) fn capability_visibility_from_definition(def: &Value) -> CapabilityVisibility {
    let raw = def
        .get("policy")
        .and_then(|p| p.get("visibility"))
        .and_then(|v| v.as_str())
        .or_else(|| def.get("visibility").and_then(|v| v.as_str()));

    raw.and_then(CapabilityVisibility::from_str)
        .unwrap_or(CapabilityVisibility::Listed)
}

pub(super) fn capability_visibility_for_cap(cap: &HubCapability) -> CapabilityVisibility {
    let def: Value = serde_json::from_str(&cap.definition).unwrap_or_else(|_| json!({}));
    capability_visibility_from_definition(&def)
}

pub(super) fn should_expose_skill_tool(cap: &HubCapability) -> bool {
    cap.enabled
        && cap.cap_type.eq_ignore_ascii_case("skill")
        && capability_visibility_for_cap(cap) == CapabilityVisibility::Listed
}

pub(super) fn should_expose_mcp_tools(cap: &HubCapability) -> bool {
    cap.enabled
        && cap.cap_type.eq_ignore_ascii_case("mcp")
        && capability_visibility_for_cap(cap) == CapabilityVisibility::Listed
}

pub(super) fn sanitize_skill_tool_name(skill_id: &str) -> Option<String> {
    let raw = skill_id.strip_prefix("skill:")?;
    let mut output = String::from("tachi_skill_");
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            output.push(c.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    Some(output.trim_end_matches('_').to_string())
}

pub(super) fn make_text_tool_result(
    payload: &Value,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    let text = serde_json::to_string(payload)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("serialize response: {e}"), None))?;
    serde_json::from_value(json!({
        "content": [{"type": "text", "text": text}],
        "isError": false
    }))
    .map_err(|e| rmcp::ErrorData::internal_error(format!("build MCP response: {e}"), None))
}

pub(super) fn build_skill_tool_from_cap(
    cap: &HubCapability,
) -> Result<(String, rmcp::model::Tool), String> {
    let tool_name = sanitize_skill_tool_name(&cap.id)
        .ok_or_else(|| format!("Invalid skill id '{}'", cap.id))?;
    let def: Value = serde_json::from_str(&cap.definition)
        .map_err(|e| format!("Invalid skill definition JSON: {e}"))?;
    let input_schema = def
        .get("inputSchema")
        .cloned()
        .or_else(|| def.get("input_schema").cloned())
        .unwrap_or(json!({
            "type": "object",
            "properties": {
                "input": {"type": "string", "description": "Primary input for this skill"},
                "context": {"type": "string", "description": "Optional context"}
            },
            "additionalProperties": true
        }));
    let description = if cap.description.is_empty() {
        format!("Run skill {}", cap.name)
    } else {
        cap.description.clone()
    };
    let tool = serde_json::from_value::<rmcp::model::Tool>(json!({
        "name": tool_name,
        "description": description,
        "inputSchema": input_schema
    }))
    .map_err(|e| format!("Build skill tool failed: {e}"))?;
    Ok((tool_name, tool))
}
