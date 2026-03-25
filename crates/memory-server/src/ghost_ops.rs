use super::*;

pub(super) async fn handle_ghost_publish(
    server: &MemoryServer,
    params: GhostPublishParams,
) -> Result<String, String> {
    let msg_id = uuid::Uuid::new_v4().to_string();
    let timestamp = Utc::now().to_rfc3339();

    let msg = PubSubMessage {
        topic: params.topic.clone(),
        payload: params.payload,
        publisher: params.publisher.clone(),
        timestamp: timestamp.clone(),
        id: msg_id.clone(),
    };

    let mut state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());
    let ring = state
        .messages
        .entry(params.topic.clone())
        .or_insert_with(VecDeque::new);
    ring.push_back(msg);
    if ring.len() > PUBSUB_RING_MAX {
        ring.pop_front();
    }
    let idx = state.next_index.entry(params.topic.clone()).or_insert(0);
    *idx += 1;

    serde_json::to_string(&json!({
        "id": msg_id,
        "topic": params.topic,
        "publisher": params.publisher,
        "timestamp": timestamp,
        "global_index": *idx,
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_ghost_subscribe(
    server: &MemoryServer,
    params: GhostSubscribeParams,
) -> Result<String, String> {
    let mut state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());

    if state.cursors.len() >= PUBSUB_MAX_CURSORS && !state.cursors.contains_key(&params.agent_id) {
        let evict_agent = state
            .cursor_recency
            .iter()
            .min_by_key(|(_, seq)| *seq)
            .map(|(agent_id, _)| agent_id.clone())
            .or_else(|| state.cursors.keys().next().cloned());

        if let Some(agent_id) = evict_agent {
            state.cursors.remove(&agent_id);
            state.cursor_recency.remove(&agent_id);
        }
    }

    let prev_cursors: HashMap<String, usize> = state
        .cursors
        .get(&params.agent_id)
        .cloned()
        .unwrap_or_default();

    let mut new_messages: Vec<serde_json::Value> = Vec::new();
    let mut new_cursors: HashMap<String, usize> = prev_cursors.clone();

    for topic in &params.topics {
        let global_idx = state.next_index.get(topic).copied().unwrap_or(0);
        let cursor = prev_cursors.get(topic).copied().unwrap_or(0);

        if cursor >= global_idx {
            new_cursors.insert(topic.clone(), global_idx);
            continue;
        }

        if let Some(ring) = state.messages.get(topic) {
            let ring_start_idx = if global_idx >= ring.len() {
                global_idx - ring.len()
            } else {
                0
            };
            let skip = if cursor > ring_start_idx {
                cursor - ring_start_idx
            } else {
                0
            };

            for msg in ring.iter().skip(skip) {
                new_messages.push(json!({
                    "id": msg.id,
                    "topic": msg.topic,
                    "payload": msg.payload,
                    "publisher": msg.publisher,
                    "timestamp": msg.timestamp,
                }));
            }
        }

        new_cursors.insert(topic.clone(), global_idx);
    }

    state.cursors.insert(params.agent_id.clone(), new_cursors);
    state.cursor_seq = state.cursor_seq.saturating_add(1);
    let recency_seq = state.cursor_seq;
    state
        .cursor_recency
        .insert(params.agent_id.clone(), recency_seq);

    serde_json::to_string(&json!({
        "agent_id": params.agent_id,
        "new_count": new_messages.len(),
        "messages": new_messages,
    }))
    .map_err(|e| format!("serialize: {e}"))
}

pub(super) async fn handle_ghost_topics(server: &MemoryServer) -> Result<String, String> {
    let state = server.pubsub.lock().unwrap_or_else(|e| e.into_inner());

    let mut topics: Vec<serde_json::Value> = Vec::new();
    for (topic, ring) in &state.messages {
        if ring.is_empty() {
            continue;
        }
        let last_msg = ring.back().unwrap();
        topics.push(json!({
            "topic": topic,
            "count": ring.len(),
            "total_published": state.next_index.get(topic).copied().unwrap_or(0),
            "last_message_time": last_msg.timestamp,
            "last_publisher": last_msg.publisher,
        }));
    }

    serde_json::to_string(&json!({
        "active_topics": topics.len(),
        "topics": topics,
    }))
    .map_err(|e| format!("serialize: {e}"))
}
