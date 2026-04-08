import path from "node:path";
import { fileURLToPath } from "node:url";
import { defaultDbPath, resolveUserPath } from "./constants.js";
const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const pluginDataDir = path.resolve(moduleDir, "data");
const workspaceRoot = process.env.OPENCLAW_WORKSPACE || "";
export const defaultConfig = {
    dbPath: process.env.MEMORY_DB_PATH
        ? resolveUserPath(process.env.MEMORY_DB_PATH)
        : defaultDbPath,
    shadowStorePath: path.resolve(pluginDataDir, "shadow-store.jsonl"),
    auditLogPath: path.resolve(pluginDataDir, "audit-log.jsonl"),
    topK: 6,
    exposeExperimentalTachiTools: process.env.TACHI_OPENCLAW_EXPERIMENTAL_TACHI_TOOLS === "1" ||
        process.env.TACHI_OPENCLAW_EXPERIMENTAL_TACHI_TOOLS === "true",
    captureMinChars: Number(process.env.MEMORY_BRIDGE_CAPTURE_MIN_CHARS || 24),
    captureTriggerKeywords: (process.env.MEMORY_BRIDGE_CAPTURE_TRIGGERS ||
        "记住,remember,偏好,喜欢,讨厌,生日,地址,电话,邮箱,习惯,计划,deadline,TODO,密码,账号,关键,always,never,重要,important")
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
    selfEvolutionAgents: ["jayne"],
    weights: {
        semantic: 0.4,
        fts: 0.3,
        symbolic: 0.2,
        decay: 0.1,
    },
};
export const bridgeConfigSchema = {
    parse(value) {
        // Start with defaults, merge any overrides from plugin config
        const overrides = (value && typeof value === "object" ? value : {});
        return {
            ...defaultConfig,
            ...overrides,
            selfEvolutionAgents: Array.isArray(overrides.selfEvolutionAgents)
                ? overrides.selfEvolutionAgents
                    .filter((value) => typeof value === "string")
                    .map((value) => value.trim())
                    .filter(Boolean)
                : defaultConfig.selfEvolutionAgents,
            weights: {
                ...defaultConfig.weights,
                ...(overrides.weights || {}),
            },
        };
    },
};
export { moduleDir as pluginDir, pluginDataDir, workspaceRoot };
