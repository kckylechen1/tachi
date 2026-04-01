import fs from "node:fs/promises";
import path from "node:path";
import { MemoryMcpClient } from "./mcp-client.js";
function asFiniteNumber(value) {
    const n = typeof value === "number" ? value : Number(value);
    return Number.isFinite(n) ? n : 0;
}
let JsMemoryStoreClass = null;
let napiLoadAttempted = false;
async function loadNapi() {
    if (napiLoadAttempted)
        return JsMemoryStoreClass;
    napiLoadAttempted = true;
    try {
        const mod = await import("@chaoxlabs/tachi-node");
        JsMemoryStoreClass = mod.JsMemoryStore;
    }
    catch {
        // Native module not available — MCP-only mode
    }
    return JsMemoryStoreClass;
}
class NapiMemoryStore {
    store;
    constructor(store) {
        this.store = store;
    }
    static create(dbPath) {
        if (!JsMemoryStoreClass)
            return null;
        try {
            return new NapiMemoryStore(new JsMemoryStoreClass(dbPath));
        }
        catch {
            return null;
        }
    }
    upsert(entry) {
        this.store.upsert(JSON.stringify(entry));
    }
    get(id) {
        const jsonStr = this.store.get(id);
        if (!jsonStr)
            return undefined;
        return JSON.parse(jsonStr);
    }
    getAll(limit) {
        const jsonStr = this.store.getAll(limit);
        if (!jsonStr)
            return [];
        return JSON.parse(jsonStr);
    }
    search(query, queryVec, opts) {
        let optionsJson = undefined;
        if (opts || queryVec) {
            const payload = { ...(opts || {}) };
            if (queryVec) {
                payload.query_vec = queryVec;
            }
            optionsJson = JSON.stringify(payload);
        }
        const resultsJson = this.store.search(query, optionsJson);
        const results = JSON.parse(resultsJson);
        const docs = [];
        const scores = {};
        const scoreBreakdowns = {};
        for (const r of results) {
            docs.push(r.entry);
            const rawScore = r.score;
            if (rawScore && typeof rawScore === "object") {
                const scoreRecord = rawScore;
                const breakdown = {
                    vector: asFiniteNumber(scoreRecord.vector),
                    fts: asFiniteNumber(scoreRecord.fts ?? scoreRecord.lexical),
                    symbolic: asFiniteNumber(scoreRecord.symbolic),
                    decay: asFiniteNumber(scoreRecord.decay),
                    final: asFiniteNumber(scoreRecord.final ?? scoreRecord.final_score),
                };
                scores[r.entry.id] = breakdown.final;
                scoreBreakdowns[r.entry.id] = breakdown;
            }
            else {
                const final = asFiniteNumber(rawScore);
                scores[r.entry.id] = final;
                scoreBreakdowns[r.entry.id] = {
                    vector: 0,
                    fts: 0,
                    symbolic: 0,
                    decay: 0,
                    final,
                };
            }
        }
        return { docs, scores, scoreBreakdowns };
    }
    findSimilar(queryVec, topK = 5) {
        const optionsJson = JSON.stringify({
            query_vec: queryVec,
            top_k: topK,
            candidates: topK,
            record_access: false,
        });
        const resultsJson = this.store.search("", optionsJson);
        const results = JSON.parse(resultsJson);
        return results
            .filter((r) => r.score.vector > 0)
            .map((r) => ({ entry: r.entry, similarity: r.score.vector }));
    }
    delete(id) {
        return this.store.delete(id);
    }
    stats() {
        return JSON.parse(this.store.stats(false));
    }
}
export class MemoryStore {
    logger;
    napiStore;
    mcpClient;
    preferredBackend;
    mcpFailedAt = null;
    static MCP_RETRY_AFTER_MS = 30_000;
    constructor(dbPath, napiStore, logger) {
        this.logger = logger;
        this.napiStore = napiStore;
        const backendRaw = (process.env.OPENCLAW_MEMORY_BACKEND || "mcp").trim().toLowerCase();
        this.preferredBackend = backendRaw === "napi" ? "napi" : "mcp";
        this.mcpClient = this.preferredBackend === "mcp" ? new MemoryMcpClient(dbPath, logger) : null;
        if (!this.napiStore && !this.mcpClient) {
            // Force MCP when NAPI is absent, regardless of env setting
            this.mcpClient = new MemoryMcpClient(dbPath, logger);
        }
    }
    isMcpAvailable() {
        if (this.mcpFailedAt === null)
            return true;
        if (Date.now() - this.mcpFailedAt > MemoryStore.MCP_RETRY_AFTER_MS) {
            this.mcpFailedAt = null;
            this.logger?.info?.("memory-hybrid-bridge: MCP backend retry window reached, attempting reconnect");
            return true;
        }
        return false;
    }
    async withBackend(operation, mcpRun, napiRun) {
        if (this.preferredBackend === "napi" && napiRun) {
            return await napiRun();
        }
        if (this.isMcpAvailable() && this.mcpClient) {
            try {
                return await mcpRun();
            }
            catch (error) {
                this.mcpFailedAt = Date.now();
                if (napiRun) {
                    this.logger?.warn?.(`memory-hybrid-bridge: MCP backend failed during ${operation}, falling back to NAPI (retry in ${MemoryStore.MCP_RETRY_AFTER_MS / 1000}s): ${String(error)}`);
                    return await napiRun();
                }
                throw error;
            }
        }
        if (napiRun) {
            return await napiRun();
        }
        throw new Error(`memory-hybrid-bridge: no backend available for ${operation} (NAPI absent, MCP unavailable)`);
    }
    getMcpClient() {
        return this.mcpClient;
    }
    async close() {
        await this.mcpClient?.close();
    }
    async upsert(entry) {
        await this.withBackend("upsert", async () => {
            await this.mcpClient.saveMemory(entry);
        }, this.napiStore
            ? () => { this.napiStore.upsert(entry); }
            : null);
    }
    async get(id) {
        return await this.withBackend("get", async () => await this.mcpClient.getMemory(id), this.napiStore
            ? () => this.napiStore.get(id)
            : null);
    }
    async getAll(limit) {
        return await this.withBackend("getAll", async () => await this.mcpClient.listMemories(limit), this.napiStore
            ? () => this.napiStore.getAll(limit)
            : null);
    }
    async search(query, queryVec, opts) {
        return await this.withBackend("search", async () => await this.mcpClient.searchMemory(query, queryVec, opts), this.napiStore
            ? () => this.napiStore.search(query, queryVec, opts)
            : null);
    }
    async findSimilar(queryVec, topK = 5) {
        return await this.withBackend("findSimilar", async () => await this.mcpClient.findSimilarMemory(queryVec, topK), this.napiStore
            ? () => this.napiStore.findSimilar(queryVec, topK)
            : null);
    }
    async delete(id) {
        return await this.withBackend("delete", async () => await this.mcpClient.deleteMemory(id), this.napiStore
            ? () => this.napiStore.delete(id)
            : null);
    }
    async stats() {
        return await this.withBackend("stats", async () => await this.mcpClient.memoryStats(), this.napiStore
            ? () => this.napiStore.stats()
            : null);
    }
}
// Auto-migration wrapper
export async function getStore(dbPath, legacyShadowPath, logger) {
    // Attempt to load native module (no-op if already tried)
    await loadNapi();
    const needsMigration = legacyShadowPath &&
        (await fs
            .stat(legacyShadowPath)
            .then((s) => s.size > 0)
            .catch(() => false)) &&
        !(await fs.stat(dbPath).catch(() => false));
    await fs.mkdir(path.dirname(dbPath), { recursive: true });
    const napiStore = NapiMemoryStore.create(dbPath);
    if (!napiStore) {
        logger?.info?.("memory-hybrid-bridge: native module unavailable, running MCP-only");
    }
    const store = new MemoryStore(dbPath, napiStore, logger);
    if (needsMigration) {
        logger?.info("memory-hybrid-bridge: initiating SQLite migration from JSONL");
        try {
            const raw = await fs.readFile(legacyShadowPath, "utf8");
            const lines = raw
                .split("\n")
                .map((l) => l.trim())
                .filter(Boolean);
            const toMigrate = [];
            for (const line of lines) {
                try {
                    const e = JSON.parse(line);
                    if (e.id || e.entry_id) {
                        // Remap old 'entry_id' -> 'id', 'lossless_restatement' -> 'text'
                        // Rust serde alias handles decoding but we rewrite here to clean
                        if (!e.path)
                            e.path = "/openclaw/legacy";
                        if (!e.summary)
                            e.summary = (e.text || e.lossless_restatement || "").substring(0, 100);
                        if (!e.importance)
                            e.importance = 0.8;
                        toMigrate.push(e);
                    }
                }
                catch (e) { }
            }
            if (toMigrate.length > 0) {
                // Just sequentially insert mapping old keys
                for (const e of toMigrate) {
                    await store.upsert(e);
                }
                logger?.info(`memory-hybrid-bridge: successfully migrated ${toMigrate.length} memories to Rust SQLite`);
                // Backup the old file so it won't be reused
                await fs
                    .rename(legacyShadowPath, `${legacyShadowPath}.migrated-${Date.now()}`)
                    .catch(() => { });
            }
        }
        catch (e) {
            logger?.warn(`memory-hybrid-bridge: migration error: ${String(e)}`);
        }
    }
    return store;
}
