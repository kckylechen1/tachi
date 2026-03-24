import type { MemoryEntry } from "./config.js";
import { type HybridScore } from "./mcp-client.js";
type LoggerLike = {
    info?: (message: string) => void;
    warn?: (message: string) => void;
};
export declare class MemoryStore {
    private readonly logger?;
    private readonly napiStore;
    private readonly mcpClient;
    private readonly preferredBackend;
    private mcpFailed;
    constructor(dbPath: string, logger?: LoggerLike | undefined);
    private withBackend;
    upsert(entry: MemoryEntry): Promise<void>;
    get(id: string): Promise<MemoryEntry | undefined>;
    getAll(limit: number): Promise<MemoryEntry[]>;
    search(query: string, queryVec?: number[], opts?: {
        top_k?: number;
        candidates?: number;
        path_prefix?: string;
        record_access?: boolean;
        weights?: {
            semantic: number;
            fts: number;
            symbolic: number;
            decay: number;
        };
    }): Promise<{
        docs: MemoryEntry[];
        scores: Record<string, number>;
        scoreBreakdowns: Record<string, HybridScore>;
    }>;
    findSimilar(queryVec: number[], topK?: number): Promise<Array<{
        entry: MemoryEntry;
        similarity: number;
    }>>;
    delete(id: string): Promise<boolean>;
    stats(): Promise<unknown>;
}
export declare function getStore(dbPath: string, legacyShadowPath?: string, logger?: any): Promise<MemoryStore>;
export {};
//# sourceMappingURL=store.d.ts.map