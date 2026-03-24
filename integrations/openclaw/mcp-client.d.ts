import type { MemoryEntry } from "./config.js";
export type HybridScore = {
    vector: number;
    fts: number;
    symbolic: number;
    decay: number;
    final: number;
};
type SearchPayload = {
    docs: MemoryEntry[];
    scores: Record<string, number>;
    scoreBreakdowns: Record<string, HybridScore>;
};
type LoggerLike = {
    info?: (message: string) => void;
    warn?: (message: string) => void;
};
type SearchOptions = {
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
};
export declare class MemoryMcpClient {
    private readonly dbPath;
    private readonly logger?;
    private client;
    private transport;
    private connecting;
    private availableTools;
    constructor(dbPath: string, logger?: LoggerLike | undefined);
    private logInfo;
    private logWarn;
    private resolveServerCommand;
    private buildLaunchCandidates;
    private connectWith;
    private getClient;
    private resetConnection;
    private callJson;
    saveMemory(entry: MemoryEntry): Promise<void>;
    getMemory(id: string): Promise<MemoryEntry | undefined>;
    listMemories(limit: number): Promise<MemoryEntry[]>;
    searchMemory(query: string, queryVec?: number[], opts?: SearchOptions): Promise<SearchPayload>;
    findSimilarMemory(queryVec: number[], topK: number): Promise<Array<{
        entry: MemoryEntry;
        similarity: number;
    }>>;
    deleteMemory(id: string): Promise<boolean>;
    memoryStats(): Promise<unknown>;
}
export {};
//# sourceMappingURL=mcp-client.d.ts.map