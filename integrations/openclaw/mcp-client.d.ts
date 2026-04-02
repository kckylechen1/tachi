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
type RecallContextOptions = {
    top_k?: number;
    candidate_multiplier?: number;
    path_prefix?: string;
    agent_id?: string;
    exclude_topics?: string[];
    min_score?: number;
};
type CompactContextParams = {
    agent_id: string;
    conversation_id: string;
    window_id: string;
    trigger?: string;
    messages: Array<{
        role: string;
        content: string;
    }>;
    current_summary?: string;
    path_prefix?: string;
    target_tokens?: number;
    max_output_tokens?: number;
    persist?: boolean;
};
type MemoryGraphParams = {
    memory_id?: string;
    query?: string;
    path_prefix?: string;
    top_k?: number;
    depth?: number;
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
    close(): Promise<void>;
    private callJson;
    saveMemory(entry: MemoryEntry): Promise<void>;
    getMemory(id: string): Promise<MemoryEntry | undefined>;
    memoryGraph(params: MemoryGraphParams): Promise<unknown>;
    listMemories(limit: number): Promise<MemoryEntry[]>;
    searchMemory(query: string, queryVec?: number[], opts?: SearchOptions): Promise<SearchPayload>;
    recallContext(query: string, opts?: RecallContextOptions): Promise<{
        prependContext: string;
        results: Array<{
            entry: MemoryEntry;
            final_score: number;
        }>;
    }>;
    captureSession(params: {
        conversation_id: string;
        turn_id: string;
        agent_id: string;
        messages: Array<{
            role: string;
            content: string;
        }>;
        path_prefix?: string;
        scope?: string;
        force?: boolean;
    }): Promise<unknown>;
    compactContext(params: CompactContextParams): Promise<{
        status: string;
        compacted_text: string;
        estimated_tokens: number;
        queued_job_ids?: string[];
    }>;
    findSimilarMemory(queryVec: number[], topK: number): Promise<Array<{
        entry: MemoryEntry;
        similarity: number;
    }>>;
    deleteMemory(id: string): Promise<boolean>;
    memoryStats(): Promise<unknown>;
    callTool(toolName: string, args: Record<string, unknown>): Promise<unknown>;
}
export {};
//# sourceMappingURL=mcp-client.d.ts.map