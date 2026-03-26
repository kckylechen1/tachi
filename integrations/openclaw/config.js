import path from "node:path";
import { fileURLToPath } from "node:url";
import { defaultDbPath, resolveUserPath } from "./constants.js";
const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const pluginDataDir = path.resolve(moduleDir, "data");
const workspaceRoot = process.env.OPENCLAW_WORKSPACE || "";
const repoRoot = path.resolve(moduleDir, "../..");
export const defaultConfig = {
    promptPath: workspaceRoot
        ? path.resolve(workspaceRoot, "scripts/memory_builder_prompt.txt")
        : path.resolve(repoRoot, "scripts/memory_builder_prompt.txt"),
    dbPath: process.env.MEMORY_DB_PATH
        ? resolveUserPath(process.env.MEMORY_DB_PATH)
        : defaultDbPath,
    shadowStorePath: path.resolve(pluginDataDir, "shadow-store.jsonl"),
    auditLogPath: path.resolve(pluginDataDir, "audit-log.jsonl"),
    topK: 6,
    searchReadLimit: Number(process.env.MEMORY_BRIDGE_SEARCH_READ_LIMIT || 2000),
    dedupThreshold: Number(process.env.MEMORY_BRIDGE_DEDUP_THRESHOLD || 0.95),
    mergeThreshold: Number(process.env.MEMORY_BRIDGE_MERGE_THRESHOLD || 0.85),
    captureMinChars: Number(process.env.MEMORY_BRIDGE_CAPTURE_MIN_CHARS || 24),
    captureTriggerKeywords: (process.env.MEMORY_BRIDGE_CAPTURE_TRIGGERS ||
        "记住,remember,偏好,喜欢,讨厌,生日,地址,电话,邮箱,习惯,计划,deadline,TODO,密码,账号,关键,always,never,重要,important")
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
        apiKey: process.env.MEMORY_BRIDGE_OPENAI_API_KEY ||
            process.env.SILICONFLOW_API_KEY ||
            process.env.OPENAI_API_KEY ||
            "",
        model: process.env.MEMORY_BRIDGE_OPENAI_MODEL || "THUDM/GLM-4-9B-0414",
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
    parse(value) {
        // Start with defaults, merge any overrides from plugin config
        const overrides = (value && typeof value === "object" ? value : {});
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
export { moduleDir as pluginDir, pluginDataDir, workspaceRoot };
