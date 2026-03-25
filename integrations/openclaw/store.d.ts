import type { MemoryEntry } from "./config.js";
import { type HybridScore } from "./mcp-client.js";
type LoggerLike = {
    info?: (message: string) => void;
    warn?: (message: string) => void;
};
interface NapiStoreBinding {
    upsert(json: string): void;
    get(id: string): string | null;
    getAll(limit: number): string | null;
    search(query: string, optionsJson?: string): string;
    delete(id: string): boolean;
    stats(verbose: boolean): string;
}
declare class NapiMemoryStore {
    private readonly store;
    constructor(store: NapiStoreBinding);
    static create(dbPath: string): NapiMemoryStore | null;
    upsert(entry: MemoryEntry): void;
    get(id: string): MemoryEntry | undefined;
    getAll(limit: number): MemoryEntry[];
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
    }): {
        docs: MemoryEntry[];
        scores: Record<string, number>;
        scoreBreakdowns: Record<string, HybridScore>;
    };
    findSimilar(queryVec: number[], topK?: number): Array<{
        entry: MemoryEntry;
        similarity: number;
    }>;
    delete(id: string): boolean;
    stats(): unknown;
}
export declare class MemoryStore {
    private readonly logger?;
    private readonly napiStore;
    private readonly mcpClient;
    private readonly preferredBackend;
    private mcpFailedAt;
    private static readonly MCP_RETRY_AFTER_MS;
    constructor(dbPath: string, napiStore: NapiMemoryStore | null, logger?: LoggerLike | undefined);
    private isMcpAvailable;
    private withBackend;
    close(): Promise<void>;
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