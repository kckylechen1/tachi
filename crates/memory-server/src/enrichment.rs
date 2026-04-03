use super::*;

// ─── Enrichment Batcher ──────────────────────────────────────────────────────

/// An item queued for background embedding + summary enrichment.
#[derive(Debug, Clone)]
pub(super) struct EnrichmentItem {
    pub(super) id: String,
    pub(super) text: String,
    pub(super) needs_embedding: bool,
    pub(super) needs_summary: bool,
    pub(super) target_db: DbScope,
    pub(super) named_project: Option<String>,
    pub(super) foundry_agent_id: Option<String>,
    pub(super) foundry_path_prefix: Option<String>,
    pub(super) revision: i64,
}

/// Batch enrichment queue configuration.
pub(super) const ENRICH_BATCH_MAX: usize = 32;
pub(super) const ENRICH_FLUSH_INTERVAL_MS: u64 = 500;

impl MemoryServer {
    /// Background worker that batches enrichment requests (embedding + summary).
    /// Flushes every ENRICH_FLUSH_INTERVAL_MS or when ENRICH_BATCH_MAX items accumulate.
    pub(super) async fn run_enrichment_batcher(
        server: MemoryServer,
        mut rx: mpsc::UnboundedReceiver<EnrichmentItem>,
    ) {
        let mut batch: Vec<EnrichmentItem> = Vec::with_capacity(ENRICH_BATCH_MAX);
        let flush_interval = Duration::from_millis(ENRICH_FLUSH_INTERVAL_MS);

        loop {
            // Wait for first item or channel close
            let item = if batch.is_empty() {
                match rx.recv().await {
                    Some(item) => Some(item),
                    None => break, // channel closed
                }
            } else {
                None
            };

            if let Some(item) = item {
                batch.push(item);
            }

            // Drain more items until batch is full or timeout expires
            let deadline = tokio::time::Instant::now() + flush_interval;
            while batch.len() < ENRICH_BATCH_MAX {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(item)) => batch.push(item),
                    Ok(None) => {
                        // Channel closed; flush remaining and exit
                        if !batch.is_empty() {
                            server.flush_enrichment_batch(&mut batch).await;
                        }
                        return;
                    }
                    Err(_timeout) => break, // timer expired, flush what we have
                }
            }

            if !batch.is_empty() {
                server.flush_enrichment_batch(&mut batch).await;
            }
        }

        eprintln!("[enrichment-batcher] channel closed, worker exiting");
    }

    /// Flush a batch: batch-embed all texts needing embedding, then update DB.
    pub(super) async fn flush_enrichment_batch(&self, batch: &mut Vec<EnrichmentItem>) {
        let items: Vec<EnrichmentItem> = batch.drain(..).collect();
        let batch_size = items.len();
        eprintln!("[enrichment-batcher] flushing batch of {batch_size} items");

        // 1. Batch embedding for items that need it
        let embed_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.needs_embedding)
            .map(|(i, _)| i)
            .collect();

        let embed_texts: Vec<String> = embed_indices
            .iter()
            .map(|&i| items[i].text.clone())
            .collect();

        let mut embed_results: Vec<Option<Vec<f32>>> = vec![None; items.len()];

        if !embed_texts.is_empty() {
            match self.llm.embed_voyage_batch(&embed_texts, "document").await {
                Ok(vecs) => {
                    for (vec_idx, &item_idx) in embed_indices.iter().enumerate() {
                        if vec_idx < vecs.len() {
                            embed_results[item_idx] = Some(vecs[vec_idx].clone());
                        }
                    }
                    eprintln!(
                        "[enrichment-batcher] batch embedded {} texts in 1 API call",
                        embed_texts.len()
                    );
                }
                Err(e) => {
                    eprintln!("[enrichment-batcher] batch embedding failed: {e}");
                }
            }
        }

        // 2. Generate summaries concurrently for items that need them
        let summary_futures: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.needs_summary)
            .map(|(i, item)| {
                let llm = self.llm.clone();
                let text = item.text.clone();
                async move { (i, llm.generate_summary(&text).await) }
            })
            .collect();

        let summary_results: Vec<(usize, Result<String, String>)> =
            futures::future::join_all(summary_futures).await;

        let mut summaries: Vec<Option<String>> = vec![None; items.len()];
        for (idx, result) in summary_results {
            match result {
                Ok(s) => summaries[idx] = Some(s),
                Err(e) => eprintln!(
                    "[enrichment-batcher] summary failed for {}: {e}",
                    items[idx].id
                ),
            }
        }

        // 3. Write results back to DB
        for (i, item) in items.iter().enumerate() {
            let new_vec = embed_results[i].as_deref();
            let new_summary = summaries[i].as_deref();

            if new_vec.is_some() || new_summary.is_some() {
                let update_action = |store: &mut MemoryStore| {
                    store
                        .update_enrichment_fields(&item.id, new_summary, new_vec, item.revision)
                        .map_err(|e| format!("Failed to update enriched entry: {e}"))
                };

                let res = if let Some(ref project_name) = item.named_project {
                    self.with_named_project_store(project_name, update_action)
                } else {
                    self.with_store_for_scope(item.target_db, update_action)
                };

                match res {
                    Ok(true) => {
                        if new_vec.is_some() {
                            if let (Some(agent_id), Some(path_prefix)) = (
                                item.foundry_agent_id.as_deref(),
                                item.foundry_path_prefix.as_deref(),
                            ) {
                                if let Err(err) = enqueue_foundry_capture_maintenance(
                                    self,
                                    item.target_db,
                                    item.named_project.clone(),
                                    agent_id,
                                    path_prefix,
                                    &[item.id.clone()],
                                ) {
                                    eprintln!(
                                        "[enrichment-batcher] failed to enqueue foundry maintenance for {}: {err}",
                                        item.id
                                    );
                                }
                            }
                        }
                    }
                    Ok(false) => eprintln!(
                        "[enrichment-batcher] discarded {} (revision changed)",
                        item.id
                    ),
                    Err(e) => {
                        eprintln!("[enrichment-batcher] DB update failed for {}: {e}", item.id)
                    }
                }
            }
        }

        eprintln!("[enrichment-batcher] batch of {batch_size} complete");
    }
}
