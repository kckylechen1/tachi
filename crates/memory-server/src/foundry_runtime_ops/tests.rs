use super::maintenance::memory_claim_signature;
use super::recall::parse_session_capture_response;
use super::*;

#[test]
fn parse_session_capture_response_accepts_json_array() {
    let raw = r#"[{"text": "hello"}, {"text": "world"}]"#;
    let drafts = parse_session_capture_response(raw).unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].text, "hello");
    assert_eq!(drafts[1].text, "world");
}

#[test]
fn parse_session_capture_response_strips_code_fence() {
    let raw = "```json\n[{\"text\": \"hello\"}]\n```";
    let drafts = parse_session_capture_response(raw).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].text, "hello");
}

#[test]
fn parse_session_capture_response_filters_empty_text() {
    let raw = r#"[{"text": ""}, {"text": "valid"}]"#;
    let drafts = parse_session_capture_response(raw).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].text, "valid");
}

#[test]
fn memory_claim_signature_changes_on_revision() {
    let entry = MemoryEntry {
        id: "test".to_string(),
        path: "/test".to_string(),
        summary: "test".to_string(),
        text: "test".to_string(),
        importance: 0.5,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        category: "fact".to_string(),
        topic: "".to_string(),
        keywords: vec![],
        persons: vec![],
        entities: vec![],
        location: "".to_string(),
        source: "test".to_string(),
        scope: "project".to_string(),
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata: json!({}),
        vector: None,
    };

    let before = memory_claim_signature(&entry);
    let mut entry2 = entry.clone();
    entry2.revision = 2;
    let after = memory_claim_signature(&entry2);
    assert_ne!(before, after);
}

#[test]
fn memory_claim_signature_changes_on_vector() {
    let mut entry = MemoryEntry {
        id: "test".to_string(),
        path: "/test".to_string(),
        summary: "test".to_string(),
        text: "test".to_string(),
        importance: 0.5,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        category: "fact".to_string(),
        topic: "".to_string(),
        keywords: vec![],
        persons: vec![],
        entities: vec![],
        location: "".to_string(),
        source: "test".to_string(),
        scope: "project".to_string(),
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata: json!({}),
        vector: None,
    };

    let before = memory_claim_signature(&entry);
    entry.vector = Some(vec![0.1, 0.2]);
    let after = memory_claim_signature(&entry);
    assert_ne!(before, after);
}

#[test]
fn forget_sweep_keeps_newest_distill_entries() {
    let mut entries = vec![
        MemoryEntry {
            id: "old".to_string(),
            path: "/foundry/agents/main/distilled/20260402T000000".to_string(),
            summary: "old".to_string(),
            text: "old".to_string(),
            importance: 0.7,
            timestamp: "2026-04-02T00:00:00Z".to_string(),
            category: "other".to_string(),
            topic: "foundry_distill".to_string(),
            keywords: vec![],
            persons: vec![],
            entities: vec![],
            location: "".to_string(),
            source: FOUNDRY_DISTILL_SOURCE.to_string(),
            scope: "project".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            metadata: json!({}),
            vector: None,
        },
        MemoryEntry {
            id: "new".to_string(),
            path: "/foundry/agents/main/distilled/20260402T010000".to_string(),
            summary: "new".to_string(),
            text: "new".to_string(),
            importance: 0.7,
            timestamp: "2026-04-02T01:00:00Z".to_string(),
            category: "other".to_string(),
            topic: "foundry_distill".to_string(),
            keywords: vec![],
            persons: vec![],
            entities: vec![],
            location: "".to_string(),
            source: FOUNDRY_DISTILL_SOURCE.to_string(),
            scope: "project".to_string(),
            archived: false,
            access_count: 0,
            last_access: None,
            revision: 1,
            metadata: json!({}),
            vector: None,
        },
    ];
    entries.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.path.cmp(&a.path))
            .then_with(|| b.id.cmp(&a.id))
    });

    assert_eq!(entries[0].id, "new");
    assert_eq!(entries[1].id, "old");
}
