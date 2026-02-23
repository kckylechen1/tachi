import path from "node:path";
import { Type, type Static } from "@sinclair/typebox";

// ============================================================================
// Memory Entry Schema (matches Phase 2 architecture)
// ============================================================================

export type SourceRef = {
  ref_type: "turn" | "file" | "message" | "event" | "other";
  ref_id: string;
  start?: number;
  end?: number;
  note?: string;
};

export type MemoryCategory = "preference" | "fact" | "decision" | "entity" | "other";

export type MemoryEntry = {
  id: string; // From entry_id
  text: string; // From lossless_restatement
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

// ============================================================================
// Plugin Config Schema (OpenClaw plugin API compatible)
// ============================================================================

export type BridgeConfig = {
  dbPath: string;
  shadowStorePath: string; // Keep for migration only
  auditLogPath: string;
  promptPath: string;
  topK: number;
  searchReadLimit: number;
  dedupThreshold: number;
  captureMinChars: number;
  captureTriggerKeywords: string[];
  weights: { semantic: number; fts: number; symbolic: number; decay: number };
  extractor: {
    baseUrl: string;
    apiKey: string;
    model: string;
    timeoutMs: number;
  };
  embedding: {
    baseUrl: string;
    apiKey: string;
    model: string;
    dimension: number;
  };
};

const workspace = process.env.OPENCLAW_WORKSPACE || process.cwd();

export const defaultConfig: BridgeConfig = {
  promptPath: path.resolve(workspace, "scripts/memory_builder_prompt.txt"),
  dbPath: path.resolve(workspace, "extensions/memory-hybrid-bridge/data/memory.db"),
  shadowStorePath: path.resolve(
    workspace,
    "extensions/memory-hybrid-bridge/data/shadow-store.jsonl",
  ),
  auditLogPath: path.resolve(workspace, "extensions/memory-hybrid-bridge/data/audit-log.jsonl"),
  topK: 6,
  searchReadLimit: Number(process.env.MEMORY_BRIDGE_SEARCH_READ_LIMIT || 2000),
  dedupThreshold: Number(process.env.MEMORY_BRIDGE_DEDUP_THRESHOLD || 0.9),
  captureMinChars: Number(process.env.MEMORY_BRIDGE_CAPTURE_MIN_CHARS || 24),
  captureTriggerKeywords: (
    process.env.MEMORY_BRIDGE_CAPTURE_TRIGGERS ||
    "记住,remember,偏好,喜欢,讨厌,生日,地址,电话,邮箱,习惯,计划,deadline,TODO,密码,账号,关键,always,never,重要,important"
  )
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean),
  weights: {
    semantic: 0.4,
    fts: 0.3,
    symbolic: 0.2,
    decay: 0.1,
  },
  extractor: {
    baseUrl: process.env.MEMORY_BRIDGE_OPENAI_BASE_URL || "https://api.siliconflow.cn/v1",
    apiKey:
      process.env.MEMORY_BRIDGE_OPENAI_API_KEY ||
      process.env.SILICONFLOW_API_KEY ||
      process.env.OPENAI_API_KEY ||
      "",
    model: process.env.MEMORY_BRIDGE_OPENAI_MODEL || "Qwen/Qwen3-8B",
    timeoutMs: Number(process.env.MEMORY_BRIDGE_OPENAI_TIMEOUT_MS || 25000),
  },
  embedding: {
    baseUrl: process.env.MEMORY_BRIDGE_EMBEDDING_BASE_URL || "https://api.voyageai.com/v1",
    apiKey: process.env.VOYAGE_API_KEY || "",
    model: process.env.MEMORY_BRIDGE_EMBEDDING_MODEL || "voyage-4",
    dimension: Number(process.env.MEMORY_BRIDGE_EMBEDDING_DIMENSION || 1024),
  },
};

export const bridgeConfigSchema = {
  parse(value: unknown): BridgeConfig {
    // Start with defaults, merge any overrides from plugin config
    const overrides = (value && typeof value === "object" ? value : {}) as Partial<BridgeConfig>;
    return {
      ...defaultConfig,
      ...overrides,
      weights: {
        ...defaultConfig.weights,
        ...(overrides.weights || {}),
      },
      extractor: {
        ...defaultConfig.extractor,
        ...(overrides.extractor || {}),
      },
      embedding: {
        ...defaultConfig.embedding,
        ...(overrides.embedding || {}),
      },
    };
  },
};
