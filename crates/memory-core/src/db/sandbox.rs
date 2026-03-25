use rusqlite::{params, Connection};

use crate::error::MemoryError;

use super::common::now_utc_iso;

// ─── Semantic Sandboxing (Role-Based Memory Isolation) ───────────────────────

/// Set (upsert) a sandbox access rule for a given agent role and path pattern.
pub fn set_sandbox_rule(
    conn: &Connection,
    agent_role: &str,
    path_pattern: &str,
    access_level: &str,
) -> Result<(), MemoryError> {
    let now = now_utc_iso();
    conn.execute(
        "INSERT INTO sandbox_rules (agent_role, path_pattern, access_level, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_role, path_pattern) DO UPDATE SET
             access_level = excluded.access_level,
             created_at = excluded.created_at",
        params![agent_role, path_pattern, access_level, &now],
    )?;
    Ok(())
}

/// Check if an agent role can access a given path for a specific operation.
/// Returns (allowed, matching_rule_description).
/// Logic: "deny" overrides everything (checked across ALL matching rules).
/// Among non-deny rules, the most specific match wins. Default = allow if no rule matches.
pub fn check_sandbox_access(
    conn: &Connection,
    agent_role: &str,
    path: &str,
    operation: &str,
) -> Result<(bool, Option<String>), MemoryError> {
    // Fetch all rules for this role, ordered by specificity (longest pattern first)
    let mut stmt = conn.prepare(
        "SELECT path_pattern, access_level FROM sandbox_rules WHERE agent_role = ?1 ORDER BY LENGTH(path_pattern) DESC, path_pattern ASC"
    )?;
    let rows = stmt.query_map(params![agent_role], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut best_non_deny: Option<(String, String, usize)> = None;

    for row in rows {
        let (pattern, access_level) = row?;
        if path_matches_pattern(path, &pattern) {
            // deny overrides everything — return immediately
            if access_level == "deny" {
                let rule_desc = format!("{}:{} -> deny", agent_role, pattern);
                return Ok((false, Some(rule_desc)));
            }
            // Track best non-deny match by specificity
            let specificity = pattern.len();
            let is_better = match &best_non_deny {
                None => true,
                Some((_, _, best_spec)) => specificity > *best_spec,
            };
            if is_better {
                best_non_deny = Some((pattern, access_level, specificity));
            }
        }
    }

    match best_non_deny {
        None => {
            // No rule matches — default: allow
            Ok((true, None))
        }
        Some((pattern, access_level, _)) => {
            let rule_desc = format!("{}:{} -> {}", agent_role, pattern, access_level);
            if operation == "write" && access_level == "read" {
                Ok((false, Some(rule_desc)))
            } else {
                Ok((true, Some(rule_desc)))
            }
        }
    }
}

/// Simple path pattern matching.
/// Supports:
/// - Exact match: "/finance/reports" matches "/finance/reports"
/// - Wildcard suffix: "/finance/*" matches "/finance/anything"
/// - Prefix match: "/finance" matches "/finance" and "/finance/sub"
fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    if pattern == "*" || pattern == "/*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // Wildcard: matches prefix and any sub-paths
        path == prefix || path.starts_with(&format!("{}/", prefix))
    } else if pattern.ends_with('*') {
        // Glob-style: "/foo*" matches anything starting with "/foo"
        let prefix = &pattern[..pattern.len() - 1];
        path.starts_with(prefix)
    } else {
        // Exact match or prefix match
        path == pattern || path.starts_with(&format!("{}/", pattern))
    }
}
