export type SourceRef = {
    ref_type: "turn" | "file" | "message" | "event" | "other";
    ref_id: string;
    start?: number;
    end?: number;
    note?: string;
};
export type MemoryCategory = "preference" | "fact" | "decision" | "entity" | "other";
export type MemoryEntry = {
    id: string;
    text: string;
    summary: string;
    keywords: string[];
    timestamp: string;
    location: string;
    persons: string[];
    entities: string[];
    topic: string;
    scope: string;
    path: string;
    category: MemoryCategory;
    importance: number;
    access_count: number;
    last_access: string | null;
    vector?: number[];
    metadata: {
        source_refs: SourceRef[];
        [key: string]: any;
    };
};
export type BridgeConfig = {
    dbPath: string;
    shadowStorePath: string;
    auditLogPath: string;
    topK: number;
    captureMinChars: number;
    captureTriggerKeywords: string[];
    weights: {
        semantic: number;
        fts: number;
        symbolic: number;
        decay: number;
    };
};
declare const moduleDir: string;
declare const pluginDataDir: string;
declare const workspaceRoot: string;
export declare const defaultConfig: BridgeConfig;
export declare const bridgeConfigSchema: {
    parse(value: unknown): BridgeConfig;
};
export { moduleDir as pluginDir, pluginDataDir, workspaceRoot };
//# sourceMappingURL=config.d.ts.map