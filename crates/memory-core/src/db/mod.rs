mod agent_state;
mod audit;
mod common;
mod ghost;
mod graph;
mod hub_db;
mod memory_crud;
mod sandbox;
mod schema;
mod sqlite_vec;
mod state;
mod stats_gc;

pub use agent_state::{get_agent_known_revisions, update_agent_known_state};
pub use audit::{audit_log_insert, audit_log_list};
pub use common::normalize_utc_iso_or_now;
pub use ghost::{
    ghost_fetch_messages_since, ghost_get_cursor, ghost_get_message, ghost_get_message_topic_index,
    ghost_get_topic_total, ghost_insert_reflection, ghost_list_topics, ghost_mark_message_promoted,
    ghost_publish_message, ghost_set_cursor, ghost_upsert_subscription,
};
pub use graph::{
    add_edge, avg_importance, count_same_topic, get_contradiction_count, get_edges, graph_expand,
    remove_edge, remove_edges_for_memory,
};
pub use hub_db::{
    hub_delete, hub_get, hub_get_active_version_route, hub_list, hub_record_call_outcome,
    hub_record_feedback, hub_search, hub_set_active_version_route, hub_set_enabled, hub_set_review,
    hub_upsert,
};
pub use memory_crud::{
    archive_memory, delete, fetch_by_ids, get_access_times, get_all, is_event_processed,
    list_by_path, mark_event_processed, record_access, release_event_claim, search_fts, search_vec,
    try_claim_event, update_enrichment_fields, update_with_revision, upsert,
};
pub use sandbox::{
    check_sandbox_access, get_sandbox_policy, insert_sandbox_exec_audit, list_sandbox_exec_audit,
    list_sandbox_policies, set_sandbox_policy, set_sandbox_rule,
};
pub use schema::init_schema;
pub use sqlite_vec::{register_sqlite_vec, serialize_f32, try_load_sqlite_vec};
pub use state::{
    count_derived_by_source, get_state, list_derived_by_source, save_derived, set_state,
};
pub use stats_gc::{gc_tables, stats};

#[cfg(test)]
pub(crate) use common::now_utc_iso;

#[cfg(test)]
mod tests;
