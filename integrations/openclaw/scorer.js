export function normalize(v) {
    if (!Number.isFinite(v))
        return 0;
    if (v < 0)
        return 0;
    if (v > 1)
        return 1;
    return v;
}
/**
 * 计算余弦相似度 (Cosine Similarity)
 */
export function cosineSimilarity(a, b) {
    if (a.length !== b.length)
        return 0;
    let dotProduct = 0;
    let mA = 0;
    let mB = 0;
    for (let i = 0; i < a.length; i++) {
        dotProduct += a[i] * b[i];
        mA += a[i] * a[i];
        mB += b[i] * b[i];
    }
    const mag = Math.sqrt(mA) * Math.sqrt(mB);
    return mag === 0 ? 0 : dotProduct / mag;
}
export function hybridScore(semantic, fts, weights = { semantic: 0.6, fts: 0.4 }) {
    const docIds = new Set([...Object.keys(semantic), ...Object.keys(fts)]);
    const out = {};
    for (const docId of docIds) {
        const s = normalize(semantic[docId] ?? 0);
        const f = normalize(fts[docId] ?? 0);
        const final = weights.semantic * s + weights.fts * f;
        out[docId] = {
            semantic: Number(s.toFixed(6)),
            lexical: Number(f.toFixed(6)), // backward compatible name
            symbolic: Number(f.toFixed(6)), // backward compatible name
            final_score: Number(final.toFixed(6)),
        };
    }
    return out;
}
export function rankHybrid(semantic, fts, topK = 6, weights = { semantic: 0.6, fts: 0.4 }) {
    const merged = hybridScore(semantic, fts, weights);
    return Object.entries(merged)
        .map(([docId, detail]) => ({ docId, ...detail }))
        .sort((a, b) => b.final_score - a.final_score)
        .slice(0, Math.max(topK, 1));
}
