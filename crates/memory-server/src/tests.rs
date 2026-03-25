use super::*;

fn ensure_test_env() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("VOYAGE_API_KEY", "test-voyage-key");
        std::env::set_var("SILICONFLOW_API_KEY", "test-siliconflow-key");
        std::env::set_var("SILICONFLOW_MODEL", "test-model");
        std::env::set_var("SUMMARY_MODEL", "test-summary-model");
    });
}

fn make_server() -> MemoryServer {
    ensure_test_env();
    let db_path = std::env::temp_dir().join(format!(
        "memory-server-test-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    MemoryServer::new(db_path, None).expect("failed to create test server")
}

fn make_entry(id: &str) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        path: "/".to_string(),
        summary: "".to_string(),
        text: "test memory".to_string(),
        importance: 0.7,
        timestamp: Utc::now().to_rfc3339(),
        category: "fact".to_string(),
        topic: "".to_string(),
        keywords: Vec::new(),
        persons: Vec::new(),
        entities: Vec::new(),
        location: "".to_string(),
        source: "test".to_string(),
        scope: "general".to_string(),
        archived: false,
        access_count: 0,
        last_access: None,
        revision: 1,
        metadata: json!({}),
        vector: None,
    }
}

fn make_test_tool(name: &str) -> rmcp::model::Tool {
    serde_json::from_value(json!({
        "name": name,
        "description": format!("tool {name}"),
        "inputSchema": {
            "type": "object",
            "additionalProperties": true,
        }
    }))
    .expect("failed to build test tool")
}

#[tokio::test]
async fn proxy_call_blocks_disabled_capability_even_directly() {
    let server = make_server();

    let cap = HubCapability {
        id: "mcp:blocked".to_string(),
        cap_type: "mcp".to_string(),
        name: "blocked".to_string(),
        version: 1,
        description: "test disabled server".to_string(),
        definition: r#"{"transport":"stdio","command":"npx","args":[]}"#.to_string(),
        enabled: false,
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: String::new(),
        updated_at: String::new(),
    };

    server
        .with_global_store(|store| {
            store
                .hub_register(&cap)
                .map_err(|e| format!("register failed: {e}"))
        })
        .expect("failed to register capability");

    let err = server
        .proxy_call_internal("blocked", "some_tool", None)
        .await
        .expect_err("disabled MCP capability should be blocked");

    assert!(
        err.to_string().contains("disabled"),
        "expected disabled error, got: {}",
        err
    );
}

#[tokio::test]
async fn ghost_subscribe_evicts_least_recent_cursor_when_full() {
    let server = make_server();

    {
        let mut state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..PUBSUB_MAX_CURSORS {
            let agent_id = format!("agent-{i}");
            state.cursors.insert(agent_id.clone(), HashMap::new());
            state.cursor_recency.insert(agent_id, (i as u64) + 1);
        }
        state.cursor_seq = PUBSUB_MAX_CURSORS as u64;
    }

    let params = GhostSubscribeParams {
        agent_id: "agent-new".to_string(),
        topics: vec![],
    };

    let _ = server
        .ghost_subscribe(Parameters(params))
        .await
        .expect("ghost_subscribe should succeed");

    let state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(state.cursors.len(), PUBSUB_MAX_CURSORS);
    assert!(
        !state.cursors.contains_key("agent-0"),
        "expected least-recent agent to be evicted"
    );
    assert!(state.cursors.contains_key("agent-new"));
    assert!(state.cursor_recency.contains_key("agent-new"));
}

#[tokio::test]
async fn sync_memories_errors_if_agent_state_persist_fails() {
    let server = make_server();

    server
        .with_global_store(|store| {
            store
                .upsert(&make_entry("sync-1"))
                .map_err(|e| format!("upsert failed: {e}"))
        })
        .expect("failed to seed memory");

    server
        .with_global_store(|store| {
            store
                .connection()
                .execute_batch(
                    r#"
                    DROP TRIGGER IF EXISTS block_agent_known_state_insert;
                    CREATE TRIGGER block_agent_known_state_insert
                    BEFORE INSERT ON agent_known_state
                    BEGIN
                        SELECT RAISE(FAIL, 'blocked by test');
                    END;
                    "#,
                )
                .map_err(|e| format!("trigger setup failed: {e}"))
        })
        .expect("failed to install blocking trigger");

    let params = SyncMemoriesParams {
        agent_id: "agent-sync-test".to_string(),
        path_prefix: Some("/".to_string()),
        limit: 10,
    };

    let err = server
        .sync_memories(Parameters(params))
        .await
        .expect_err("sync_memories should fail when state persistence fails");

    assert!(
        err.contains("failed to persist agent state"),
        "unexpected error: {err}"
    );
}

#[test]
fn filter_mcp_tools_respects_allow_and_deny_permissions() {
    let def = json!({
        "permissions": {
            "allow": ["echo", "add"],
            "deny": ["add"],
        }
    });

    let filtered = filter_mcp_tools_by_permissions(
        &def,
        vec![
            make_test_tool("echo"),
            make_test_tool("add"),
            make_test_tool("secret"),
        ],
    );

    let names: Vec<String> = filtered
        .iter()
        .map(|tool| tool.name.as_ref().to_string())
        .collect();
    assert_eq!(names, vec!["echo"]);
}

#[tokio::test]
async fn hub_register_discovery_failure_disables_capability_and_clears_proxy_tools() {
    let server = make_server();
    let params = HubRegisterParams {
        id: "mcp:discovery-fails".to_string(),
        cap_type: "mcp".to_string(),
        name: "discovery-fails".to_string(),
        description: "test discovery failure".to_string(),
        definition: json!({
            "transport": "stdio",
            "command": "/usr/bin/true",
            "args": [],
        })
        .to_string(),
        version: 1,
        scope: "global".to_string(),
    };

    let response = server
        .hub_register(Parameters(params))
        .await
        .expect("hub_register should return response");
    let data: serde_json::Value =
        serde_json::from_str(&response).expect("hub_register response should be JSON");

    assert_eq!(data.get("enabled"), Some(&json!(false)));
    assert!(
        data.get("discovery_error")
            .and_then(|v| v.as_str())
            .is_some(),
        "expected discovery_error in response: {data}"
    );

    let cap = server
        .get_capability("mcp:discovery-fails")
        .expect("capability should be persisted");
    assert!(
        !cap.enabled,
        "capability should be disabled after discovery failure"
    );

    let def: serde_json::Value =
        serde_json::from_str(&cap.definition).expect("stored definition should be valid JSON");
    assert_eq!(def.get("discovery_status"), Some(&json!("failed")));
    assert!(
        def.get("last_discovery_error")
            .and_then(|v| v.as_str())
            .is_some(),
        "stored definition should include last_discovery_error"
    );

    let proxy_tools = server.proxy_tools.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        !proxy_tools.contains_key("discovery-fails"),
        "failed capability should not leave proxy tools cached"
    );
}

#[test]
fn resolve_mcp_tool_exposure_supports_definition_overrides() {
    let flatten = resolve_mcp_tool_exposure(
        &json!({"tool_exposure": "flatten"}),
        McpToolExposureMode::Gateway,
    );
    let gateway = resolve_mcp_tool_exposure(
        &json!({"tool_exposure": "gateway"}),
        McpToolExposureMode::Flatten,
    );
    let expose_false = resolve_mcp_tool_exposure(
        &json!({"expose_tools": false}),
        McpToolExposureMode::Flatten,
    );
    let fallback_default = resolve_mcp_tool_exposure(&json!({}), McpToolExposureMode::Gateway);

    assert_eq!(flatten, McpToolExposureMode::Flatten);
    assert_eq!(gateway, McpToolExposureMode::Gateway);
    assert_eq!(expose_false, McpToolExposureMode::Gateway);
    assert_eq!(fallback_default, McpToolExposureMode::Gateway);
}

#[tokio::test]
async fn retry_dispatch_blocks_direct_proxy_tool_when_gateway_mode() {
    let server = make_server();

    let cap = HubCapability {
        id: "mcp:gateway-only".to_string(),
        cap_type: "mcp".to_string(),
        name: "gateway-only".to_string(),
        version: 1,
        description: "gateway mode mcp".to_string(),
        definition: json!({
            "transport": "stdio",
            "command": "npx",
            "args": ["-y", "dummy-mcp"],
            "tool_exposure": "gateway",
        })
        .to_string(),
        enabled: true,
        uses: 0,
        successes: 0,
        failures: 0,
        avg_rating: 0.0,
        last_used: None,
        created_at: String::new(),
        updated_at: String::new(),
    };

    server
        .with_global_store(|store| {
            store
                .hub_register(&cap)
                .map_err(|e| format!("register failed: {e}"))
        })
        .expect("failed to register gateway capability");

    let err = server
        .retry_dispatch(
            "gateway-only__echo",
            Some(serde_json::Map::from_iter([(
                "text".to_string(),
                json!("hello"),
            )])),
        )
        .await
        .expect_err("gateway mode should block direct proxy tool names");

    assert!(
        err.to_string().contains("tool_exposure=gateway"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn post_card_check_inbox_and_update_roundtrip() {
    std::env::set_var("KANBAN_CLASSIFY_ENABLED", "false");
    let server = make_server();

    let posted = server
        .post_card(Parameters(PostCardParams {
            from_agent: "hapi".to_string(),
            to_agent: "iris".to_string(),
            title: "Need review".to_string(),
            body: "Please review PR #42".to_string(),
            priority: "high".to_string(),
            card_type: "request".to_string(),
            thread_id: Some("thread-42".to_string()),
        }))
        .await
        .expect("post_card should succeed");
    let posted_json: serde_json::Value =
        serde_json::from_str(&posted).expect("post_card response should be JSON");
    let card_id = posted_json["card_id"]
        .as_str()
        .expect("post_card should return card_id")
        .to_string();

    let inbox = server
        .check_inbox(Parameters(CheckInboxParams {
            agent_id: "iris".to_string(),
            status_filter: Some("open".to_string()),
            since: None,
            include_broadcast: true,
            limit: 20,
        }))
        .await
        .expect("check_inbox should succeed");
    let inbox_json: serde_json::Value =
        serde_json::from_str(&inbox).expect("check_inbox response should be JSON");
    let cards = inbox_json["cards"]
        .as_array()
        .expect("check_inbox should return cards array");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], json!(card_id));
    assert_eq!(cards[0]["status"], json!("open"));

    let updated = server
        .update_card(Parameters(UpdateCardParams {
            card_id: card_id.clone(),
            new_status: "acknowledged".to_string(),
            response_text: Some("Got it".to_string()),
        }))
        .await
        .expect("update_card should succeed");
    let updated_json: serde_json::Value =
        serde_json::from_str(&updated).expect("update_card response should be JSON");
    assert_eq!(updated_json["updated"], json!(true));

    let inbox_after = server
        .check_inbox(Parameters(CheckInboxParams {
            agent_id: "iris".to_string(),
            status_filter: Some("acknowledged".to_string()),
            since: None,
            include_broadcast: true,
            limit: 20,
        }))
        .await
        .expect("check_inbox after update should succeed");
    let inbox_after_json: serde_json::Value =
        serde_json::from_str(&inbox_after).expect("check_inbox after update should be JSON");
    let cards_after = inbox_after_json["cards"]
        .as_array()
        .expect("cards_after should be array");
    assert_eq!(cards_after.len(), 1);
    assert_eq!(cards_after[0]["status"], json!("acknowledged"));
    assert!(
        cards_after[0]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("Got it"),
        "response text should be appended to body"
    );
}

#[tokio::test]
async fn check_inbox_respects_broadcast_toggle() {
    std::env::set_var("KANBAN_CLASSIFY_ENABLED", "false");
    let server = make_server();

    server
        .post_card(Parameters(PostCardParams {
            from_agent: "aegis".to_string(),
            to_agent: "*".to_string(),
            title: "Fleet alert".to_string(),
            body: "CI pipeline blocked".to_string(),
            priority: "critical".to_string(),
            card_type: "alert".to_string(),
            thread_id: None,
        }))
        .await
        .expect("post_card broadcast should succeed");

    let no_broadcast = server
        .check_inbox(Parameters(CheckInboxParams {
            agent_id: "iris".to_string(),
            status_filter: None,
            since: None,
            include_broadcast: false,
            limit: 10,
        }))
        .await
        .expect("check_inbox without broadcast should succeed");
    let no_broadcast_json: serde_json::Value =
        serde_json::from_str(&no_broadcast).expect("check_inbox response should be JSON");
    assert_eq!(no_broadcast_json["count"], json!(0));

    let with_broadcast = server
        .check_inbox(Parameters(CheckInboxParams {
            agent_id: "iris".to_string(),
            status_filter: None,
            since: None,
            include_broadcast: true,
            limit: 10,
        }))
        .await
        .expect("check_inbox with broadcast should succeed");
    let with_broadcast_json: serde_json::Value =
        serde_json::from_str(&with_broadcast).expect("check_inbox response should be JSON");
    assert_eq!(with_broadcast_json["count"], json!(1));
}
