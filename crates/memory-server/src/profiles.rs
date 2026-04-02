use rmcp::model::Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolProfile {
    Ide,
    Runtime,
    Workflow,
    Admin,
}

impl ToolProfile {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ide => "ide",
            Self::Runtime => "runtime",
            Self::Workflow => "workflow",
            Self::Admin => "admin",
        }
    }
}

const IDE_TOOL_PATTERNS: &[&str] = &[
    "recommend_capability",
    "recommend_skill",
    "recommend_toolchain",
    "search_memory",
    "save_memory",
    "get_memory",
    "list_memories",
    "memory_stats",
    "get_edges",
];

const RUNTIME_EXTRA_TOOL_PATTERNS: &[&str] = &[
    "compact_context",
    "recall_context",
    "capture_session",
    "archive_memory",
    "delete_memory",
    "extract_facts",
    "find_similar_memory",
    "get_pipeline_status",
    "ingest_event",
    "sync_memories",
];

const WORKFLOW_TOOL_PATTERNS: &[&str] = &[
    "check_inbox",
    "ghost_*",
    "handoff_check",
    "handoff_leave",
    "post_card",
    "project_agent_profile",
    "queue_agent_evolution",
    "review_agent_evolution_proposal",
    "list_agent_evolution_proposals",
    "update_card",
];

pub(super) fn parse_tool_profile(raw: &str) -> Option<ToolProfile> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "admin" | "full" => Some(ToolProfile::Admin),
        "ide" | "agent" | "claude" | "claude-code" | "codex" | "cursor" | "trae"
        | "antigravity" => Some(ToolProfile::Ide),
        "runtime" | "openclaw" | "adapter" => Some(ToolProfile::Runtime),
        "workflow" | "ops" => Some(ToolProfile::Workflow),
        _ => None,
    }
}

pub(super) fn parse_tool_patterns_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn filter_tool_defs(
    tools: Vec<Tool>,
    profile: Option<ToolProfile>,
    env_patterns: Option<&[String]>,
) -> Vec<Tool> {
    tools
        .into_iter()
        .filter(|tool| tool_visible(tool.name.as_ref(), profile, env_patterns))
        .collect()
}

fn tool_visible(
    tool_name: &str,
    profile: Option<ToolProfile>,
    env_patterns: Option<&[String]>,
) -> bool {
    if let Some(patterns) = env_patterns {
        if !matches_any_pattern(tool_name, patterns.iter().map(String::as_str)) {
            return false;
        }
    }

    match profile.unwrap_or(ToolProfile::Admin) {
        ToolProfile::Admin => true,
        ToolProfile::Ide => matches_any_pattern(tool_name, IDE_TOOL_PATTERNS.iter().copied()),
        ToolProfile::Runtime => {
            matches_any_pattern(tool_name, IDE_TOOL_PATTERNS.iter().copied())
                || matches_any_pattern(tool_name, RUNTIME_EXTRA_TOOL_PATTERNS.iter().copied())
        }
        ToolProfile::Workflow => {
            matches_any_pattern(tool_name, WORKFLOW_TOOL_PATTERNS.iter().copied())
        }
    }
}

fn matches_any_pattern<'a>(tool_name: &str, mut patterns: impl Iterator<Item = &'a str>) -> bool {
    patterns.any(|pattern| tool_name_matches_pattern(tool_name, pattern))
}

pub(super) fn tool_name_matches_pattern(tool_name: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return tool_name == pattern;
    }

    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let segments: Vec<&str> = pattern
        .split('*')
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.is_empty() {
        return true;
    }

    let mut cursor = 0usize;
    let mut first = true;

    for segment in segments {
        let Some(found_at) = tool_name[cursor..].find(segment) else {
            return false;
        };
        let absolute = cursor + found_at;
        if first && anchored_start && absolute != 0 {
            return false;
        }
        cursor = absolute + segment.len();
        first = false;
    }

    if anchored_end {
        cursor == tool_name.len()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tool(name: &str) -> Tool {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": format!("tool {name}"),
            "inputSchema": {
                "type": "object",
                "additionalProperties": true,
            }
        }))
        .expect("test tool")
    }

    #[test]
    fn profile_parsing_maps_host_aliases() {
        assert_eq!(parse_tool_profile("codex"), Some(ToolProfile::Ide));
        assert_eq!(parse_tool_profile("openclaw"), Some(ToolProfile::Runtime));
        assert_eq!(parse_tool_profile("workflow"), Some(ToolProfile::Workflow));
        assert_eq!(parse_tool_profile("admin"), Some(ToolProfile::Admin));
    }

    #[test]
    fn pattern_matching_supports_wildcards() {
        assert!(tool_name_matches_pattern("ghost_publish", "ghost_*"));
        assert!(tool_name_matches_pattern("hub_call", "hub_*"));
        assert!(!tool_name_matches_pattern("save_memory", "ghost_*"));
    }

    #[test]
    fn profile_filter_and_env_whitelist_form_intersection() {
        let filtered = filter_tool_defs(
            vec![
                test_tool("search_memory"),
                test_tool("save_memory"),
                test_tool("recall_context"),
                test_tool("ghost_publish"),
            ],
            Some(ToolProfile::Runtime),
            Some(&["search_memory".to_string(), "recall_*".to_string()]),
        );
        let names: Vec<String> = filtered
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["search_memory".to_string(), "recall_context".to_string()]
        );
    }
}
