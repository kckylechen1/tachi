/**
 * Voyage rerank-2.5 client for memory-hybrid-bridge.
 * Reranks search results after hybrid retrieval to improve precision.
 * Falls back to original order on any failure.
 */

const RERANK_MODEL = "rerank-2.5";

/**
 * Rerank search results using Voyage rerank-2.5.
 * @param {Object} params
 * @param {Object} params.config - Bridge config (needs embedding.baseUrl and embedding.apiKey)
 * @param {string} params.query - The search query text
 * @param {Array} params.results - Array of {docId, final_score, entry} from hybrid search
 * @param {number} [params.topK=6] - Number of top results to return after reranking
 * @param {Object} [params.logger] - Logger instance
 * @returns {Array} Reranked results with rerank_score injected
 */
export async function rerank({ config, query, results, topK = 6, logger }) {
    if (!results || results.length === 0) return results;

    const apiKey = config.embedding.apiKey;
    if (!apiKey) {
        logger?.warn("memory-hybrid-bridge: no embedding API key, skipping rerank");
        return results.slice(0, topK);
    }

    // Build documents array from entries' lossless_restatement
    const documents = results.map((r) => {
        const e = r.entry;
        // Combine key fields for richer reranking context
        return [
            e.text,
            e.topic ? `Topic: ${e.topic}` : "",
            e.keywords?.length ? `Keywords: ${e.keywords.join(", ")}` : "",
        ]
            .filter(Boolean)
            .join("\n");
    });

    try {
        const baseUrl = config.embedding.baseUrl.replace(/\/$/, "");
        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), 8000); // 8s timeout

        try {
            const res = await fetch(`${baseUrl}/rerank`, {
                method: "POST",
                headers: {
                    "content-type": "application/json",
                    authorization: `Bearer ${apiKey}`,
                },
                body: JSON.stringify({
                    model: RERANK_MODEL,
                    query,
                    documents,
                    top_k: topK,
                }),
                signal: controller.signal,
            });

            if (!res.ok) {
                logger?.warn(`memory-hybrid-bridge: rerank API returned ${res.status}`);
                return results.slice(0, topK);
            }

            const data = await res.json();

            // Rebuild results in reranked order, injecting relevance_score
            const reranked = [];
            for (const item of data.data) {
                const original = results[item.index];
                reranked.push({
                    ...original,
                    rerank_score: item.relevance_score,
                    final_score: item.relevance_score, // Override hybrid score with reranker's
                });
            }

            logger?.info(
                `memory-hybrid-bridge: reranked ${results.length} → top ${reranked.length} results`,
            );
            return reranked;
        } finally {
            clearTimeout(timer);
        }
    } catch (err) {
        logger?.warn(`memory-hybrid-bridge: rerank failed, using hybrid scores: ${String(err)}`);
        return results.slice(0, topK);
    }
}
