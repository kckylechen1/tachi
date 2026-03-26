export type ScoreMap = Record<string, number>;
export type HybridScoreDetail = {
    semantic: number;
    lexical: number;
    symbolic: number;
    final_score: number;
};
export declare function normalize(v: number): number;
/**
 * 计算余弦相似度 (Cosine Similarity)
 */
export declare function cosineSimilarity(a: number[], b: number[]): number;
export declare function hybridScore(semantic: ScoreMap, fts: ScoreMap, weights?: {
    semantic: number;
    fts: number;
}): Record<string, HybridScoreDetail>;
export declare function rankHybrid(semantic: ScoreMap, fts: ScoreMap, topK?: number, weights?: {
    semantic: number;
    fts: number;
}): Array<{
    docId: string;
} & HybridScoreDetail>;
//# sourceMappingURL=scorer.d.ts.map