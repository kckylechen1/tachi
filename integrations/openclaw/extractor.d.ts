import type { BridgeConfig, MemoryEntry } from "./config.js";
export declare class SigilError extends Error {
    code: string;
    constructor(code: string, message: string);
}
export declare function sanitizeForExtractorInput(input: string): string;
export declare function validateMemoryEntry(obj: unknown): obj is MemoryEntry;
export declare function loadPromptTemplate(promptPath: string): Promise<string>;
export declare function extractMemoryEntry(params: {
    config: BridgeConfig;
    inputWindowText: string;
    sourceRefId: string;
    agentId: string;
    logger?: {
        info: (...args: any[]) => void;
        warn: (...args: any[]) => void;
    };
}): Promise<MemoryEntry | null>;
export declare function getEmbedding(params: {
    config: BridgeConfig;
    text: string;
    logger?: {
        info: (...args: any[]) => void;
        warn: (...args: any[]) => void;
    };
}): Promise<number[] | null>;
export declare function mergeMemoryEntries(params: {
    config: BridgeConfig;
    existing: MemoryEntry;
    incoming: MemoryEntry;
    logger?: {
        info: (...args: any[]) => void;
        warn: (...args: any[]) => void;
    };
}): Promise<MemoryEntry | null>;
//# sourceMappingURL=extractor.d.ts.map