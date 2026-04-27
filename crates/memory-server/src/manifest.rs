//! Manifest v1 — single source of truth for which memory.db files Tachi owns.
//!
//! Stored at `~/.tachi/manifest.json`. JSON (not TOML) to avoid adding a new
//! workspace dependency; comments simulated via `_comment` keys where useful.
//!
//! Schema:
//! {
//!   "schema_version": 1,
//!   "generated_at": "<ISO-8601>",
//!   "_comment": "Tachi-owned memory DBs. Do not hand-edit while server runs.",
//!   "dbs": [
//!     {
//!       "path": "/Users/.../.tachi/global/memory.db",
//!       "role": "global",            // global | project | agent | foundry | unknown
//!       "owner": "tachi",            // tachi | openclaw-agent:<name> | antigravity | external
//!       "schema_kind": "tachi",      // tachi | openclaw_legacy | unknown
//!       "vec_enabled": true,
//!       "allow_write": true,
//!       "last_doctor_at": "<ISO-8601>",
//!       "last_classification": "healthy",
//!       "scope_hint": "global",
//!       "notes": ""
//!     }, ...
//!   ]
//! }
//!
//! Branch #2 deliverable: load/save manifest, populate from a doctor::DoctorReport,
//! lookup by role/scope_hint, and a CLI subcommand `tachi manifest` (show | init |
//! refresh). Branch #3 will route runtime save/recall through manifest lookups.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::doctor::{DbClassification, DoctorFinding, DoctorReport};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(rename = "_comment", default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    pub dbs: Vec<DbEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbEntry {
    pub path: String,
    pub role: DbRole,
    pub owner: String,
    pub schema_kind: String,
    pub vec_enabled: bool,
    pub allow_write: bool,
    pub last_doctor_at: String,
    pub last_classification: String,
    pub scope_hint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbRole {
    Global,
    Project,
    Agent,
    Foundry,
    Unknown,
}

impl Manifest {
    pub fn empty() -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            generated_at: Utc::now().to_rfc3339(),
            comment: "Tachi-owned memory DBs. Managed by `tachi doctor` / `tachi manifest`.".to_string(),
            dbs: Vec::new(),
        }
    }

    pub fn default_path(home: &Path) -> PathBuf {
        home.join(".tachi").join("manifest.json")
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = fs::read(path)?;
        let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("manifest parse: {e}"))
        })?;
        Ok(m)
    }

    pub fn load_or_empty(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|_| Self::empty())
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("manifest serialize: {e}"))
        })?;
        // Atomic-ish write via tmp + rename.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Populate from a doctor scan. Healthy + WalOrphan + LegacySchema get
    /// recorded; placeholder/backup/corrupt are excluded (they are not "owned"
    /// in the runtime sense). Existing entries' `notes` are preserved.
    pub fn populate_from_doctor(&mut self, report: &DoctorReport) {
        use std::collections::HashMap;
        let prior_notes: HashMap<String, String> = self
            .dbs
            .iter()
            .map(|e| (e.path.clone(), e.notes.clone()))
            .collect();

        let mut new_dbs: Vec<DbEntry> = Vec::new();
        for f in &report.findings {
            if !should_record(f) {
                continue;
            }
            let role = classify_role(f);
            let owner = derive_owner(&f.scope_hint);
            let allow_write = matches!(f.classification, DbClassification::Healthy);
            let entry = DbEntry {
                path: f.path.clone(),
                role,
                owner,
                schema_kind: f.schema_kind.clone(),
                vec_enabled: f.vec_rowid_count.is_some(),
                allow_write,
                last_doctor_at: report.generated_at.clone(),
                last_classification: f.classification.as_str().to_string(),
                scope_hint: f.scope_hint.clone(),
                notes: prior_notes.get(&f.path).cloned().unwrap_or_default(),
            };
            new_dbs.push(entry);
        }
        new_dbs.sort_by(|a, b| a.path.cmp(&b.path));
        self.dbs = new_dbs;
        self.generated_at = Utc::now().to_rfc3339();
        if self.schema_version == 0 {
            self.schema_version = MANIFEST_SCHEMA_VERSION;
        }
    }

    /// Look up a single owned DB by exact path. Used by runtime resolvers in branch #3.
    pub fn lookup(&self, path: &str) -> Option<&DbEntry> {
        self.dbs.iter().find(|e| e.path == path)
    }

    /// Look up the global DB entry (if any). At most one expected per manifest.
    pub fn global(&self) -> Option<&DbEntry> {
        self.dbs.iter().find(|e| e.role == DbRole::Global)
    }

    /// All entries with a given role.
    pub fn by_role(&self, role: DbRole) -> Vec<&DbEntry> {
        self.dbs.iter().filter(|e| e.role == role).collect()
    }
}

fn should_record(f: &DoctorFinding) -> bool {
    // Only record DBs that look like Tachi/OpenClaw memory stores. Random
    // sqlite files (kline_cache.db, rust_gateway.db, run snapshots, etc.) are
    // not "owned" by Tachi runtime and must not be in the manifest.
    let class_ok = matches!(
        f.classification,
        DbClassification::Healthy | DbClassification::WalOrphan | DbClassification::LegacySchema
    );
    let schema_ok = matches!(f.schema_kind.as_str(), "tachi" | "openclaw_legacy");
    class_ok && schema_ok
}

fn classify_role(f: &DoctorFinding) -> DbRole {
    let s = f.scope_hint.as_str();
    if s == "global" {
        DbRole::Global
    } else if s.starts_with("project:") {
        DbRole::Project
    } else if s.starts_with("openclaw-agent") {
        DbRole::Agent
    } else if f.path.contains("/foundry/") || f.path.contains("/foundry.db") {
        DbRole::Foundry
    } else if s.starts_with("antigravity") {
        // Antigravity DB is rescued in branch #6 → routed under projects/. Mark as project for now.
        DbRole::Project
    } else {
        DbRole::Unknown
    }
}

fn derive_owner(scope_hint: &str) -> String {
    if let Some(rest) = scope_hint.strip_prefix("openclaw-agent:") {
        return format!("openclaw-agent:{}", rest);
    }
    if let Some(rest) = scope_hint.strip_prefix("openclaw-agent-local:") {
        return format!("openclaw-agent:{}", rest);
    }
    if scope_hint.starts_with("antigravity") {
        return "antigravity".to_string();
    }
    if scope_hint.starts_with("project:") || scope_hint == "global" {
        return "tachi".to_string();
    }
    "external".to_string()
}

pub fn render_manifest(m: &Manifest) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "tachi manifest v{}  generated_at={}  dbs={}",
        m.schema_version,
        m.generated_at,
        m.dbs.len()
    );
    for e in &m.dbs {
        let _ = writeln!(
            out,
            "  [{}] {}  owner={}  schema={}  vec={}  write={}  last={}  scope={}",
            role_str(&e.role),
            e.path,
            e.owner,
            e.schema_kind,
            e.vec_enabled,
            e.allow_write,
            e.last_classification,
            e.scope_hint,
        );
    }
    out
}

fn role_str(r: &DbRole) -> &'static str {
    match r {
        DbRole::Global => "global",
        DbRole::Project => "project",
        DbRole::Agent => "agent",
        DbRole::Foundry => "foundry",
        DbRole::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{JobBreakdown, SummaryByClass};
    use tempfile::tempdir;

    fn mk_finding(path: &str, class: DbClassification, scope: &str) -> DoctorFinding {
        DoctorFinding {
            path: path.to_string(),
            classification: class,
            file_size: 4096,
            has_wal: false,
            has_shm: false,
            mem_count: Some(1),
            archived_count: Some(0),
            vec_rowid_count: Some(1),
            none_domain_count: Some(0),
            jobs: JobBreakdown::default(),
            schema_kind: "tachi".to_string(),
            error: None,
            scope_hint: scope.to_string(),
        }
    }

    fn mk_report(findings: Vec<DoctorFinding>) -> DoctorReport {
        DoctorReport {
            scanned_roots: vec![],
            findings,
            summary: SummaryByClass::default(),
            auto_fix_actions: vec![],
            quarantine_dir: Some("/tmp/q".to_string()),
            generated_at: "2026-04-28T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn populate_filters_and_classifies_roles() {
        let mut m = Manifest::empty();
        let report = mk_report(vec![
            mk_finding("/u/.tachi/global/memory.db", DbClassification::Healthy, "global"),
            mk_finding("/u/.tachi/projects/quant/memory.db", DbClassification::Healthy, "project:quant"),
            mk_finding("/u/.openclaw/extensions/tachi/data/agents/main/memory.db", DbClassification::Healthy, "openclaw-agent:main"),
            mk_finding("/u/.tachi/junk.db", DbClassification::Placeholder, "tachi-other"),
            mk_finding("/u/.tachi/foo.db.bak", DbClassification::Backup, "tachi-other"),
            mk_finding("/u/.tachi/dead.db", DbClassification::Corrupt, "tachi-other"),
        ]);
        m.populate_from_doctor(&report);
        assert_eq!(m.dbs.len(), 3, "only healthy/wal/legacy should be recorded");
        assert_eq!(m.global().map(|e| e.path.as_str()), Some("/u/.tachi/global/memory.db"));
        assert_eq!(m.by_role(DbRole::Project).len(), 1);
        assert_eq!(m.by_role(DbRole::Agent).len(), 1);
        let agent = m.by_role(DbRole::Agent)[0];
        assert_eq!(agent.owner, "openclaw-agent:main");
    }

    #[test]
    fn save_and_load_roundtrip_preserves_notes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let mut m = Manifest::empty();
        m.populate_from_doctor(&mk_report(vec![mk_finding(
            "/u/.tachi/global/memory.db",
            DbClassification::Healthy,
            "global",
        )]));
        m.dbs[0].notes = "primary global store".to_string();
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.dbs.len(), 1);
        assert_eq!(loaded.dbs[0].notes, "primary global store");

        // Re-populating should preserve the note.
        let mut m2 = loaded.clone();
        m2.populate_from_doctor(&mk_report(vec![mk_finding(
            "/u/.tachi/global/memory.db",
            DbClassification::Healthy,
            "global",
        )]));
        assert_eq!(m2.dbs[0].notes, "primary global store");
    }

    #[test]
    fn allow_write_only_for_healthy() {
        let mut m = Manifest::empty();
        m.populate_from_doctor(&mk_report(vec![
            mk_finding("/u/a.db", DbClassification::Healthy, "project:a"),
            mk_finding("/u/b.db", DbClassification::WalOrphan, "project:b"),
            mk_finding("/u/c.db", DbClassification::LegacySchema, "project:c"),
        ]));
        let by_path: std::collections::HashMap<_, _> =
            m.dbs.iter().map(|e| (e.path.clone(), e.allow_write)).collect();
        assert_eq!(by_path["/u/a.db"], true);
        assert_eq!(by_path["/u/b.db"], false);
        assert_eq!(by_path["/u/c.db"], false);
    }

    #[test]
    fn lookup_returns_entry() {
        let mut m = Manifest::empty();
        m.populate_from_doctor(&mk_report(vec![mk_finding(
            "/u/.tachi/global/memory.db",
            DbClassification::Healthy,
            "global",
        )]));
        assert!(m.lookup("/u/.tachi/global/memory.db").is_some());
        assert!(m.lookup("/u/missing.db").is_none());
    }
}
