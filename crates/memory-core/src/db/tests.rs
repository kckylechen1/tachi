use super::*;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde_json::json;

use crate::types::{MemoryEdge, MemoryEntry};

fn make_conn() -> Connection {
    libsimple::enable_auto_extension().unwrap();
    register_sqlite_vec();
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    try_load_sqlite_vec(&conn);
    conn
}

fn make_entry(id: &str, text: &str) -> MemoryEntry {
    MemoryEntry {
        id: id.into(),
        path: "/test".into(),
        summary: text[..text.len().min(30)].into(),
        text: text.into(),
        importance: 0.7,
        timestamp: Utc::now().to_rfc3339(),
        category: "fact".into(),
        topic: "".into(),
        keywords: vec!["test".into()],
        persons: vec![],
        entities: vec![],
        location: "".into(),
        source: "".into(),
        scope: "general".into(),
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata: json!({ "keywords": ["test"], "entities": [] }),
        vector: None,
    }
}

#[test]
fn upsert_and_fts() {
    let mut conn = make_conn();
    let e = make_entry("abc", "Rust is a systems programming language");
    upsert(&mut conn, &e, false).unwrap();

    let results = search_fts(&conn, "systems programming", 5, false).unwrap();
    assert!(results.contains_key("abc"), "expected 'abc' in FTS results");
}

#[test]
fn upsert_idempotent() {
    let mut conn = make_conn();
    let mut e = make_entry("dup", "first text");
    upsert(&mut conn, &e, false).unwrap();
    e.text = "updated text".into();
    upsert(&mut conn, &e, false).unwrap();

    let results = search_fts(&conn, "updated", 5, false).unwrap();
    assert!(results.contains_key("dup"));
}

#[test]
fn processed_events_dedup_by_worker() {
    let conn = make_conn();
    assert!(!is_event_processed(&conn, "abc", "ingest").unwrap());
    mark_event_processed(&conn, "abc", "conv:1", "ingest").unwrap();
    assert!(is_event_processed(&conn, "abc", "ingest").unwrap());
    assert!(!is_event_processed(&conn, "abc", "causal").unwrap());
}

#[test]
fn update_with_revision_detects_conflict() {
    let mut conn = make_conn();
    let e = make_entry("rev-1", "original");
    upsert(&mut conn, &e, false).unwrap();

    let metadata = serde_json::to_string(&json!({"source":"test"})).unwrap();
    let ok = update_with_revision(
        &mut conn,
        "rev-1",
        "merged",
        "merged",
        "consolidation",
        &metadata,
        None,
        1,
    )
    .unwrap();
    assert!(ok);

    let stale = update_with_revision(
        &mut conn,
        "rev-1",
        "stale",
        "stale",
        "consolidation",
        &metadata,
        None,
        1,
    )
    .unwrap();
    assert!(!stale);
}

#[test]
fn search_vec_knn_with_k_constraint() {
    let mut conn = make_conn();
    let has_vec: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'memories_vec'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if has_vec == 0 {
        return;
    }

    let mut e = make_entry("vec-1", "vector memory entry");
    e.vector = Some(vec![0.1_f32; 1024]);
    upsert(&mut conn, &e, true).unwrap();

    let query = vec![0.1_f32; 1024];
    let results = search_vec(&conn, &query, 3, false).unwrap();
    assert!(results.contains_key("vec-1"));
}

#[test]
fn delete_existing() {
    let mut conn = make_conn();
    let e = make_entry("del-1", "to be deleted");
    upsert(&mut conn, &e, false).unwrap();

    let deleted = delete(&mut conn, "del-1", false).unwrap();
    assert!(deleted, "should return true for existing entry");

    // Verify it's gone from main table
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE id = 'del-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);

    // Verify it's gone from FTS
    let fts_results = search_fts(&conn, "deleted", 5, false).unwrap();
    assert!(!fts_results.contains_key("del-1"));
}

#[test]
fn delete_nonexistent() {
    let mut conn = make_conn();
    let deleted = delete(&mut conn, "nonexistent-id", false).unwrap();
    assert!(!deleted, "should return false for non-existent entry");
}

#[test]
fn stats_aggregation() {
    let mut conn = make_conn();

    let mut e1 = make_entry("s1", "fact entry");
    e1.scope = "general".into();
    e1.category = "fact".into();
    e1.path = "/project/alpha".into();
    upsert(&mut conn, &e1, false).unwrap();

    let mut e2 = make_entry("s2", "decision entry");
    e2.scope = "project".into();
    e2.category = "decision".into();
    e2.path = "/project/beta".into();
    upsert(&mut conn, &e2, false).unwrap();

    let mut e3 = make_entry("s3", "user preference");
    e3.scope = "user".into();
    e3.category = "preference".into();
    e3.path = "/user/settings".into();
    upsert(&mut conn, &e3, false).unwrap();

    let s = stats(&conn, false).unwrap();
    assert_eq!(s.total, 3);
    assert_eq!(s.by_scope.get("general"), Some(&1_u64));
    assert_eq!(s.by_scope.get("project"), Some(&1_u64));
    assert_eq!(s.by_scope.get("user"), Some(&1_u64));
    assert_eq!(s.by_category.get("fact"), Some(&1_u64));
    assert_eq!(s.by_category.get("decision"), Some(&1_u64));
    assert_eq!(s.by_root_path.get("/project"), Some(&2_u64));
    assert_eq!(s.by_root_path.get("/user"), Some(&1_u64));
}

#[test]
fn graph_add_and_get_edges() {
    let mut conn = make_conn();
    let e1 = make_entry("g1", "cause event");
    let e2 = make_entry("g2", "effect event");
    upsert(&mut conn, &e1, false).unwrap();
    upsert(&mut conn, &e2, false).unwrap();

    let edge = MemoryEdge {
        source_id: "g1".into(),
        target_id: "g2".into(),
        relation: "causes".into(),
        weight: 0.9,
        metadata: serde_json::json!({}),
        created_at: String::new(),
        valid_from: String::new(),
        valid_to: None,
    };
    add_edge(&conn, &edge).unwrap();

    let out = get_edges(&conn, "g1", "outgoing", None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].target_id, "g2");
    assert_eq!(out[0].relation, "causes");

    let inc = get_edges(&conn, "g2", "incoming", None).unwrap();
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0].source_id, "g1");
}

#[test]
fn graph_expand_bfs() {
    let mut conn = make_conn();
    // Create chain: a -> b -> c
    for id in &["a", "b", "c", "d"] {
        upsert(&mut conn, &make_entry(id, &format!("node {}", id)), false).unwrap();
    }
    add_edge(
        &conn,
        &MemoryEdge {
            source_id: "a".into(),
            target_id: "b".into(),
            relation: "follows".into(),
            weight: 1.0,
            metadata: serde_json::json!({}),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        },
    )
    .unwrap();
    add_edge(
        &conn,
        &MemoryEdge {
            source_id: "b".into(),
            target_id: "c".into(),
            relation: "follows".into(),
            weight: 1.0,
            metadata: serde_json::json!({}),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        },
    )
    .unwrap();
    // d is disconnected

    // Expand 1 hop from "a"
    let r1 = graph_expand(&conn, &["a".into()], 1, None).unwrap();
    assert_eq!(r1.entries.len(), 1); // should find b
    assert!(r1.distances.contains_key("b"));
    assert!(!r1.distances.contains_key("c")); // c is 2 hops

    // Expand 2 hops from "a"
    let r2 = graph_expand(&conn, &["a".into()], 2, None).unwrap();
    assert_eq!(r2.entries.len(), 2); // b and c
    assert!(r2.distances.contains_key("c"));
    assert!(!r2.distances.contains_key("d")); // d is disconnected
}

#[test]
fn delete_cascades_edges() {
    let mut conn = make_conn();
    let e1 = make_entry("del-e1", "source");
    let e2 = make_entry("del-e2", "target");
    upsert(&mut conn, &e1, false).unwrap();
    upsert(&mut conn, &e2, false).unwrap();
    add_edge(
        &conn,
        &MemoryEdge {
            source_id: "del-e1".into(),
            target_id: "del-e2".into(),
            relation: "causes".into(),
            weight: 1.0,
            metadata: serde_json::json!({}),
            created_at: String::new(),
            valid_from: String::new(),
            valid_to: None,
        },
    )
    .unwrap();

    delete(&mut conn, "del-e1", false).unwrap();
    let edges = get_edges(&conn, "del-e2", "both", None).unwrap();
    assert!(edges.is_empty(), "edges should be cleaned up on delete");
}

#[test]
fn delete_cascades_access_history_and_known_state() {
    let mut conn = make_conn();
    let e = make_entry("del-cascade", "delete target");
    upsert(&mut conn, &e, false).unwrap();

    record_access(&mut conn, &["del-cascade".to_string()]).unwrap();
    update_agent_known_state(
        &conn,
        "agent-delete-test",
        &[("del-cascade".to_string(), 1)],
    )
    .unwrap();

    let ah_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM access_history WHERE memory_id = ?1",
            params!["del-cascade"],
            |row| row.get(0),
        )
        .unwrap();
    let aks_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_known_state WHERE memory_id = ?1",
            params!["del-cascade"],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ah_before > 0);
    assert!(aks_before > 0);

    delete(&mut conn, "del-cascade", false).unwrap();

    let ah_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM access_history WHERE memory_id = ?1",
            params!["del-cascade"],
            |row| row.get(0),
        )
        .unwrap();
    let aks_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_known_state WHERE memory_id = ?1",
            params!["del-cascade"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ah_after, 0, "access_history should be cleaned up on delete");
    assert_eq!(
        aks_after, 0,
        "agent_known_state should be cleaned up on delete"
    );
}

#[test]
fn gc_tables_prunes_retention_and_orphans() {
    let mut conn = make_conn();
    let e = make_entry("gc-keep", "gc target");
    upsert(&mut conn, &e, false).unwrap();

    for _ in 0..300 {
        conn.execute(
            "INSERT INTO access_history (memory_id, accessed_at, query_hash) VALUES (?1, ?2, ?3)",
            params!["gc-keep", now_utc_iso(), ""],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO access_history (memory_id, accessed_at, query_hash) VALUES (?1, ?2, ?3)",
        params!["gc-orphan", now_utc_iso(), ""],
    )
    .unwrap();

    conn.execute(
            "INSERT INTO processed_events (event_hash, event_id, worker, created_at) VALUES (?1, ?2, ?3, ?4)",
            params!["ev-old", "id-old", "ingest", "2000-01-01T00:00:00.000Z"],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO processed_events (event_hash, event_id, worker, created_at) VALUES (?1, ?2, ?3, ?4)",
            params!["ev-new", "id-new", "ingest", "2999-01-01T00:00:00.000Z"],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO audit_log (timestamp, server_id, tool_name, args_hash, success, duration_ms, error_kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "2000-01-01T00:00:00.000Z",
                "mcp:test",
                "tool_old",
                "",
                1,
                1,
                Option::<String>::None,
                "2000-01-01T00:00:00.000Z"
            ],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO audit_log (timestamp, server_id, tool_name, args_hash, success, duration_ms, error_kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "2999-01-01T00:00:00.000Z",
                "mcp:test",
                "tool_new",
                "",
                1,
                1,
                Option::<String>::None,
                "2999-01-01T00:00:00.000Z"
            ],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO agent_known_state (agent_id, memory_id, revision, synced_at) VALUES (?1, ?2, ?3, ?4)",
            params!["agent-old", "gc-keep", 1, "2000-01-01T00:00:00.000Z"],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO agent_known_state (agent_id, memory_id, revision, synced_at) VALUES (?1, ?2, ?3, ?4)",
            params!["agent-new", "gc-keep", 2, "2999-01-01T00:00:00.000Z"],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO agent_known_state (agent_id, memory_id, revision, synced_at) VALUES (?1, ?2, ?3, ?4)",
            params!["agent-orphan", "gc-orphan", 1, "2999-01-01T00:00:00.000Z"],
        )
        .unwrap();

    let summary = gc_tables(&mut conn).unwrap();

    let kept_access: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM access_history WHERE memory_id = ?1",
            params!["gc-keep"],
            |row| row.get(0),
        )
        .unwrap();
    let orphan_access: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM access_history WHERE memory_id = ?1",
            params!["gc-orphan"],
            |row| row.get(0),
        )
        .unwrap();
    let processed_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM processed_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let audit_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
        .unwrap();
    let known_state_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_known_state", [], |row| {
            row.get(0)
        })
        .unwrap();
    let orphan_known_state: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agent_known_state WHERE memory_id = ?1",
            params!["gc-orphan"],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        kept_access, 256,
        "access_history should retain latest 256 per memory"
    );
    assert_eq!(orphan_access, 0, "orphaned access rows should be removed");
    assert_eq!(
        processed_count, 1,
        "old processed_events row should be pruned"
    );
    assert_eq!(audit_count, 1, "old audit_log row should be pruned");
    assert_eq!(
        known_state_count, 1,
        "old + orphan known-state rows should be pruned"
    );
    assert_eq!(
        orphan_known_state, 0,
        "orphaned known-state rows should be removed"
    );

    assert!(summary["access_history_pruned"].as_u64().unwrap_or(0) > 0);
    assert!(summary["orphaned_agent_known_state"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn sandbox_policy_crud_roundtrip() {
    let conn = make_conn();

    set_sandbox_policy(
        &conn,
        "mcp:alpha",
        "process",
        r#"["PATH"]"#,
        r#"["/safe/read"]"#,
        r#"["/safe/write"]"#,
        r#"["/work"]"#,
        10_000,
        30_000,
        2,
        true,
    )
    .unwrap();

    set_sandbox_policy(
        &conn,
        "mcp:beta",
        "container",
        r#"["HOME"]"#,
        r#"["/ro"]"#,
        r#"["/rw"]"#,
        r#"["/sandbox"]"#,
        20_000,
        40_000,
        1,
        false,
    )
    .unwrap();

    let alpha = get_sandbox_policy(&conn, "mcp:alpha").unwrap().unwrap();
    assert_eq!(alpha["capability_id"], "mcp:alpha");
    assert_eq!(alpha["runtime_type"], "process");
    assert_eq!(alpha["enabled"], true);
    assert_eq!(alpha["env_allowlist"], json!(["PATH"]));

    let enabled = list_sandbox_policies(&conn, true, 10).unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0]["capability_id"], "mcp:alpha");

    let limited = list_sandbox_policies(&conn, false, 1).unwrap();
    assert_eq!(limited.len(), 1);
}
