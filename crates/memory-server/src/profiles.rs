use rmcp::model::Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBundle {
    Observe,
    Remember,
    Coordinate,
    Operate,
    AntigravityMinimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ToolProfile {
    observe: bool,
    remember: bool,
    coordinate: bool,
    operate: bool,
    admin: bool,
    /// Antigravity host: a curated minimal allow-list intersected on top of
    /// the union of other bundles. When set, only patterns in
    /// ANTIGRAVITY_MINIMAL_TOOL_PATTERNS pass — even if other bundles would
    /// otherwise allow them. Designed to keep the Antigravity tool tray small.
    antigravity_minimal: bool,
}

impl ToolProfile {
    const fn observe() -> Self {
        Self {
            observe: true,
            remember: false,
            coordinate: false,
            operate: false,
            admin: false,
            antigravity_minimal: false,
        }
    }

    const fn remember() -> Self {
        Self {
            observe: true,
            remember: true,
            coordinate: false,
            operate: false,
            admin: false,
            antigravity_minimal: false,
        }
    }

    const fn coordinate() -> Self {
        Self {
            observe: true,
            remember: true,
            coordinate: true,
            operate: false,
            admin: false,
            antigravity_minimal: false,
        }
    }

    const fn operate() -> Self {
        Self {
            observe: true,
            remember: true,
            coordinate: false,
            operate: true,
            admin: false,
            antigravity_minimal: false,
        }
    }

    const fn admin() -> Self {
        Self {
            observe: true,
            remember: true,
            coordinate: true,
            operate: true,
            admin: true,
            antigravity_minimal: false,
        }
    }

    /// Antigravity host minimal profile: read + remember + handoff coordination,
    /// constrained by an explicit allow-list (see ANTIGRAVITY_MINIMAL_TOOL_PATTERNS).
    /// Replaces the previous behavior of mapping `antigravity` → full coordinate
    /// bundle, which exposed ~30 tools in the IDE tool tray.
    const fn antigravity() -> Self {
        Self {
            observe: true,
            remember: true,
            coordinate: true,
            operate: false,
            admin: false,
            antigravity_minimal: true,
        }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            observe: self.observe || other.observe,
            remember: self.remember || other.remember,
            coordinate: self.coordinate || other.coordinate,
            operate: self.operate || other.operate,
            admin: self.admin || other.admin,
            // Minimal allow-list is "sticky": once requested, additive merges
            // do not silently expand the surface. Use `admin`/`full` to override.
            antigravity_minimal: self.antigravity_minimal || other.antigravity_minimal,
        }
    }

    fn allows(self, bundle: ToolBundle) -> bool {
        self.admin
            || match bundle {
                ToolBundle::Observe => self.observe,
                ToolBundle::Remember => self.remember,
                ToolBundle::Coordinate => self.coordinate,
                ToolBundle::Operate => self.operate,
                ToolBundle::AntigravityMinimal => self.antigravity_minimal,
            }
    }

    pub(super) fn as_str(self) -> String {
        if self.admin {
            return "admin".to_string();
        }
        if self.antigravity_minimal {
            return "antigravity".to_string();
        }

        let mut names = Vec::new();
        if self.observe {
            names.push("observe");
        }
        if self.remember {
            names.push("remember");
        }
        if self.coordinate {
            names.push("coordinate");
        }
        if self.operate {
            names.push("operate");
        }
        if names.is_empty() {
            "observe".to_string()
        } else {
            names.join(",")
        }
    }
}

const OBSERVE_TOOL_PATTERNS: &[&str] = &[
    "tachi_task_brief",
    "tachi_progress_check",
    "tachi_wiki_search",
    "recommend_capability",
    "recommend_skill",
    "recommend_toolchain",
    "prepare_capability_bundle",
    "search_memory",
    "get_memory",
    "memory_graph",
    "list_memories",
    "memory_stats",
    "get_edges",
    "wiki_search",
    "wiki_browse",
];

const REMEMBER_TOOL_PATTERNS: &[&str] = &[
    "save_memory",
    "remember",
    "tachi_wiki_write",
    "extract_facts",
    "run_skill",
    "ingest_event",
];

const COORDINATE_TOOL_PATTERNS: &[&str] = &[
    "check_inbox",
    "ghost_*",
    "handoff_check",
    "handoff_leave",
    "post_card",
    "update_card",
];

const OPERATE_TOOL_PATTERNS: &[&str] = &[
    "section_build",
    "compact_context",
    "compact_rollup",
    "compact_session_memory",
    "recall_context",
    "capture_session",
    "archive_memory",
    "find_similar_memory",
    "get_pipeline_status",
    "sync_memories",
    "agent_register",
    "agent_whoami",
    "synthesize_agent_evolution",
    "project_agent_profile",
    "queue_agent_evolution",
    "review_agent_evolution_proposal",
    "list_agent_evolution_proposals",
    "hub_call",
    "hub_disconnect",
    "wiki_lint",
];

/// Antigravity host minimal allow-list. Intersected with the union of
/// observe+remember+coordinate bundles so the IDE tray stays small.
/// Order: read-first, then write, then handoff/coordination.
const ANTIGRAVITY_MINIMAL_TOOL_PATTERNS: &[&str] = &[
    // Discovery + lessons
    "tachi_task_brief",
    "tachi_progress_check",
    "tachi_wiki_search",
    "wiki_search",
    "wiki_browse",
    // Memory read
    "search_memory",
    "get_memory",
    "list_memories",
    // Memory write (canonical + low-friction shortcut)
    "save_memory",
    "remember",
    "tachi_wiki_write",
    // Cross-session handoff (the only coordination Antigravity needs)
    "handoff_check",
    "handoff_leave",
];

pub(super) fn parse_tool_profile(raw: &str) -> Option<ToolProfile> {
    let mut resolved: Option<ToolProfile> = None;

    for token in raw
        .split([',', '+'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let token_profile = match token.to_ascii_lowercase().as_str() {
            "observe" | "read" | "reader" => ToolProfile::observe(),
            "remember" | "write" | "writer" | "ide" | "agent" | "claude" | "claude-code"
            | "codex" | "cursor" | "trae" => ToolProfile::remember(),
            "coordinate" => ToolProfile::coordinate(),
            "antigravity" => ToolProfile::antigravity(),
            "companion" | "copilot" | "coach" => ToolProfile::remember()
                .merge(ToolProfile::coordinate())
                .merge(ToolProfile::operate()),
            "workflow" => ToolProfile::coordinate().merge(ToolProfile::operate()),
            "operate" | "runtime" | "openclaw" | "adapter" | "ops" => ToolProfile::operate(),
            "admin" | "full" => ToolProfile::admin(),
            _ => return None,
        };
        resolved = Some(match resolved {
            Some(profile) => profile.merge(token_profile),
            None => token_profile,
        });
    }

    resolved
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

    let profile = profile.unwrap_or_else(ToolProfile::admin);
    if profile.admin {
        return true;
    }

    // Antigravity host: intersect bundle membership with a curated allow-list
    // so the IDE tool tray stays small (~12 tools instead of ~30).
    if profile.allows(ToolBundle::AntigravityMinimal)
        && !matches_any_pattern(tool_name, ANTIGRAVITY_MINIMAL_TOOL_PATTERNS.iter().copied())
    {
        return false;
    }

    profile.allows(ToolBundle::Observe)
        && matches_any_pattern(tool_name, OBSERVE_TOOL_PATTERNS.iter().copied())
        || profile.allows(ToolBundle::Remember)
            && matches_any_pattern(tool_name, REMEMBER_TOOL_PATTERNS.iter().copied())
        || profile.allows(ToolBundle::Coordinate)
            && matches_any_pattern(tool_name, COORDINATE_TOOL_PATTERNS.iter().copied())
        || profile.allows(ToolBundle::Operate)
            && matches_any_pattern(tool_name, OPERATE_TOOL_PATTERNS.iter().copied())
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
        assert_eq!(parse_tool_profile("codex"), Some(ToolProfile::remember()));
        assert_eq!(parse_tool_profile("openclaw"), Some(ToolProfile::operate()));
        assert_eq!(
            parse_tool_profile("companion"),
            Some(
                ToolProfile::remember()
                    .merge(ToolProfile::coordinate())
                    .merge(ToolProfile::operate())
            )
        );
        assert_eq!(
            parse_tool_profile("antigravity"),
            Some(ToolProfile::antigravity())
        );
        assert_eq!(
            parse_tool_profile("workflow"),
            Some(ToolProfile::coordinate().merge(ToolProfile::operate()))
        );
        assert_eq!(parse_tool_profile("admin"), Some(ToolProfile::admin()));
    }

    #[test]
    fn profile_parsing_supports_additive_surface_tokens() {
        assert_eq!(
            parse_tool_profile("observe,coordinate"),
            Some(ToolProfile::observe().merge(ToolProfile::coordinate()))
        );
        assert_eq!(
            parse_tool_profile("remember+operate"),
            Some(ToolProfile::remember().merge(ToolProfile::operate()))
        );
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
            Some(ToolProfile::operate()),
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

    #[test]
    fn coordinate_surface_includes_memory_and_workflow_tools() {
        let filtered = filter_tool_defs(
            vec![
                test_tool("search_memory"),
                test_tool("save_memory"),
                test_tool("ingest_event"),
                test_tool("post_card"),
                test_tool("hub_register"),
            ],
            Some(ToolProfile::coordinate()),
            None,
        );
        let names: Vec<String> = filtered
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "search_memory".to_string(),
                "save_memory".to_string(),
                "ingest_event".to_string(),
                "post_card".to_string()
            ]
        );
    }

    #[test]
    fn explicit_remember_surface_excludes_admin_tools() {
        let filtered = filter_tool_defs(
            vec![
                test_tool("search_memory"),
                test_tool("save_memory"),
                test_tool("ingest_event"),
                test_tool("hub_register"),
            ],
            Some(ToolProfile::remember()),
            None,
        );
        let names: Vec<String> = filtered
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "search_memory".to_string(),
                "save_memory".to_string(),
                "ingest_event".to_string()
            ]
        );
    }

    #[test]
    fn omitted_profile_keeps_compatibility_admin_surface() {
        let filtered = filter_tool_defs(
            vec![
                test_tool("search_memory"),
                test_tool("save_memory"),
                test_tool("hub_register"),
            ],
            None,
            None,
        );
        let names: Vec<String> = filtered
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "search_memory".to_string(),
                "save_memory".to_string(),
                "hub_register".to_string()
            ]
        );
    }

    #[test]
    fn antigravity_profile_restricts_to_minimal_allowlist() {
        // Bundle membership says coordinate would normally include
        // ghost_publish/post_card/check_inbox; the antigravity intersection
        // must drop them.
        let filtered = filter_tool_defs(
            vec![
                test_tool("search_memory"),
                test_tool("save_memory"),
                test_tool("remember"),
                test_tool("get_memory"),
                test_tool("list_memories"),
                test_tool("tachi_task_brief"),
                test_tool("tachi_progress_check"),
                test_tool("tachi_wiki_search"),
                test_tool("tachi_wiki_write"),
                test_tool("handoff_check"),
                test_tool("handoff_leave"),
                // These should be filtered OUT by the minimal allow-list:
                test_tool("ghost_publish"),
                test_tool("post_card"),
                test_tool("check_inbox"),
                test_tool("update_card"),
                test_tool("ingest_event"),
                test_tool("memory_graph"),
                test_tool("recommend_capability"),
                test_tool("run_skill"),
            ],
            Some(ToolProfile::antigravity()),
            None,
        );
        let names: Vec<String> = filtered
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "search_memory".to_string(),
                "save_memory".to_string(),
                "remember".to_string(),
                "get_memory".to_string(),
                "list_memories".to_string(),
                "tachi_task_brief".to_string(),
                "tachi_progress_check".to_string(),
                "tachi_wiki_search".to_string(),
                "tachi_wiki_write".to_string(),
                "handoff_check".to_string(),
                "handoff_leave".to_string(),
            ]
        );
    }

    #[test]
    fn antigravity_profile_label_is_distinct() {
        assert_eq!(ToolProfile::antigravity().as_str(), "antigravity");
        // admin still wins over the minimal flag if explicitly merged.
        assert_eq!(
            ToolProfile::antigravity()
                .merge(ToolProfile::admin())
                .as_str(),
            "admin"
        );
    }
}
