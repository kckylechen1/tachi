// search.rs — Hybrid search orchestration
//
// Runs Vec + FTS + Symbolic channels, merges scores in Rust, returns top K.
// This is the hottest path: all computation stays in Rust, zero JS/Python overhead.

use rusqlite::Connection;
use std::collections::HashMap;

use crate::{
    db::{fetch_by_ids, record_access, search_fts, search_vec},
    error::MemoryError,
    scorer::{hybrid_score, symbolic_score, HybridWeights},
    types::{MemoryEntry, SearchResult},
};

/// Options for a hybrid search query.
pub struct SearchOptions {
    /// Number of candidates to pull from each channel before merging.
    pub candidates_per_channel: usize,
    /// Final top-K to return after scoring.
    pub top_k: usize,
    /// Scoring weights.
    pub weights: HybridWeights,
    /// Optionally restrict results to a path prefix (e.g. "/openclaw")
    pub path_prefix: Option<String>,
    /// Pre-computed query embedding; if None, skip vector channel.
    pub query_vec: Option<Vec<f32>>,
    /// Whether the sqlite-vec extension is available for vector search.
    pub vec_available: bool,
    /// Whether to bump access_count after retrieval (disable in bulk/bench mode).
    pub record_access: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            candidates_per_channel: 20,
            top_k: 6,
            weights: HybridWeights::default(),
            path_prefix: None,
            query_vec: None,
            vec_available: false,
            record_access: true,
        }
    }
}

/// Execute a full hybrid search, returning ranked `SearchResult`s.
///
/// Execution plan:
///  1. Vector KNN (via sqlite-vec) — if `query_vec` provided
///  2. FTS5 BM25 — always
///  3. Symbolic bag-of-words — computed in Rust on the fetched entries
///  4. Hybrid score merge (weighted sum) with ACT-R decay
///  5. Sort, take top_k, record access
pub fn hybrid_search(
    conn: &Connection,
    query: &str,
    opts: &SearchOptions,
) -> Result<Vec<SearchResult>, MemoryError> {
    let n = opts.candidates_per_channel;

    // ── Channel 1: Vector ─────────────────────────────────────────────────────
    let vec_scores: HashMap<String, f64> = if opts.vec_available {
        if let Some(qv) = &opts.query_vec {
            search_vec(conn, qv, n)?
        } else {
            HashMap::new()
        }
    } else {
        HashMap::new()
    };

    // ── Channel 2: FTS5 ───────────────────────────────────────────────────────
    let fts_scores = search_fts(conn, query, n)?;

    // ── Collect all candidate IDs ──────────────────────────────────────────────
    let candidate_ids: Vec<String> = vec_scores
        .keys()
        .chain(fts_scores.keys())
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if candidate_ids.is_empty() {
        return Ok(vec![]);
    }

    // ── Bulk-fetch entries ─────────────────────────────────────────────────────
    let entries_map = fetch_by_ids(conn, &candidate_ids)?;

    // ── Channel 3: Symbolic ───────────────────────────────────────────────────
    let symbolic_scores: HashMap<String, f64> = entries_map
        .iter()
        .map(|(id, entry)| {
            let score = symbolic_score(query, &entry.text, &entry.keywords);
            (id.clone(), score)
        })
        .collect();

    // ── Optional path-prefix filter ───────────────────────────────────────────
    let entries_ref: HashMap<String, &MemoryEntry> = if let Some(prefix) = &opts.path_prefix {
        entries_map
            .iter()
            .filter(|(_, e)| e.path.starts_with(prefix.as_str()))
            .map(|(k, v)| (k.clone(), v))
            .collect()
    } else {
        entries_map.iter().map(|(k, v)| (k.clone(), v)).collect()
    };

    // ── Hybrid scoring ─────────────────────────────────────────────────────────
    let scores = hybrid_score(
        &entries_ref,
        &vec_scores,
        &fts_scores,
        &symbolic_scores,
        &opts.weights,
    );

    // ── Sort and take top K ───────────────────────────────────────────────────
    let mut ranked: Vec<(&String, f64)> = scores
        .iter()
        .map(|(id, hs)| (id, hs.final_score))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(opts.top_k);

    // ── Build output ──────────────────────────────────────────────────────────
    let mut results: Vec<SearchResult> = ranked
        .iter()
        .filter_map(|(id, _)| {
            let entry = entries_map.get(*id)?.clone();
            let score = scores.get(*id)?.clone();
            Some(SearchResult { entry, score })
        })
        .collect();

    // ── Record access (bump counters) ─────────────────────────────────────────
    if opts.record_access {
        let accessed_ids: Vec<String> = results.iter().map(|r| r.entry.id.clone()).collect();
        record_access(conn, &accessed_ids)?;
        // Reflect the bump in returned entries
        for r in &mut results {
            r.entry.access_count += 1;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{init_schema, try_load_sqlite_vec, register_sqlite_vec, upsert};
    use crate::types::MemoryEntry;
    use chrono::Utc;
    use rusqlite::Connection;
    use serde_json::json;

    fn setup() -> Connection {
        libsimple::enable_auto_extension().unwrap();
        register_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        try_load_sqlite_vec(&conn);
        conn
    }

    fn insert(conn: &Connection, id: &str, text: &str, keywords: &[&str]) {
        let e = MemoryEntry {
            id: id.into(),
            path: "/test".into(),
            summary: text[..text.len().min(30)].into(),
            text: text.into(),
            importance: 0.7,
            timestamp: Utc::now().to_rfc3339(),
            category: "fact".into(),
            topic: "".into(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            persons: vec![],
            entities: vec![],
            location: "".into(),
            source: "".into(),
            scope: "general".into(),
            access_count: 0,
            last_access: None,
            metadata: json!({ "keywords": keywords, "entities": [] }),
            vector: None,
        };
        upsert(conn, &e, false).unwrap();
    }

    #[test]
    fn hybrid_returns_relevant() {
        let conn = setup();
        insert(&conn, "a", "Rust is fast and memory safe", &["rust", "performance"]);
        insert(&conn, "b", "Python is great for scripting", &["python", "scripting"]);

        let opts = SearchOptions {
            top_k: 3,
            record_access: false,
            ..Default::default()
        };
        let results = hybrid_search(&conn, "rust performance", &opts).unwrap();
        assert!(!results.is_empty());
        // "a" should score higher for "rust performance" query
        assert_eq!(results[0].entry.id, "a");
    }

    #[test]
    fn empty_query_returns_empty() {
        let conn = setup();
        insert(&conn, "x", "some text", &[]);
        let opts = SearchOptions { record_access: false, ..Default::default() };
        let results = hybrid_search(&conn, "", &opts).unwrap();
        // FTS5 with empty query should produce no FTS results; vec channel also empty
        assert!(results.is_empty());
    }
}
