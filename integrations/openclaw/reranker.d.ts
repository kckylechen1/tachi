import type { BridgeConfig, MemoryEntry } from "./config.js";

export function rerank(params: {
  config: BridgeConfig;
  query: string;
  results: Array<{ final_score: number; entry: MemoryEntry }>;
  topK?: number;
  logger?: { info: (...args: any[]) => void; warn: (...args: any[]) => void };
}): Promise<Array<{ final_score: number; entry: MemoryEntry; rerank_score?: number }>>;
