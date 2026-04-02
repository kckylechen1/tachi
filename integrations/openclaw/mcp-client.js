import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
const REQUIRED_TOOLS = [
    "recall_context",
    "capture_session",
    "save_memory",
    "search_memory",
    "get_memory",
    "delete_memory",
    "memory_stats",
    "list_memories",
];
function asFiniteNumber(value) {
    const n = typeof value === "number" ? value : Number(value);
    return Number.isFinite(n) ? n : 0;
}
function asString(value) {
    return typeof value === "string" ? value : "";
}
function asStringArray(value) {
    if (!Array.isArray(value)) {
        return [];
    }
    return value.filter((v) => typeof v === "string");
}
function isRecord(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
}
function isSourceRef(value) {
    return (isRecord(value) &&
        typeof value.ref_type === "string" &&
        typeof value.ref_id === "string");
}
function ensureMetadata(value) {
    if (!isRecord(value)) {
        return { source_refs: [] };
    }
    const sourceRefsRaw = value.source_refs;
    const sourceRefs = Array.isArray(sourceRefsRaw)
        ? sourceRefsRaw.filter((item) => isSourceRef(item))
        : [];
    return {
        source_refs: sourceRefs,
        ...value,
    };
}
function extractTextBlocks(content) {
    if (!Array.isArray(content)) {
        return [];
    }
    const out = [];
    for (const block of content) {
        if (!isRecord(block)) {
            continue;
        }
        if (block.type === "text" && typeof block.text === "string") {
            out.push(block.text);
        }
    }
    return out;
}
function coerceMemoryEntry(raw) {
    if (!isRecord(raw)) {
        return undefined;
    }
    const id = asString(raw.id);
    if (!id) {
        return undefined;
    }
    return {
        id,
        text: asString(raw.text),
        summary: asString(raw.summary),
        keywords: asStringArray(raw.keywords),
        timestamp: asString(raw.timestamp),
        location: asString(raw.location),
        persons: asStringArray(raw.persons),
        entities: asStringArray(raw.entities),
        topic: asString(raw.topic),
        scope: asString(raw.scope) || "general",
        path: asString(raw.path) || "/",
        category: (asString(raw.category) || "other"),
        importance: asFiniteNumber(raw.importance) || 0.7,
        access_count: asFiniteNumber(raw.access_count),
        last_access: typeof raw.last_access === "string" ? raw.last_access : null,
        vector: Array.isArray(raw.vector)
            ? raw.vector
                .map((v) => asFiniteNumber(v))
                .filter((v) => Number.isFinite(v))
            : undefined,
        metadata: ensureMetadata(raw.metadata),
    };
}
function extractErrorMessage(result, toolName) {
    const text = extractTextBlocks(result.content).find(Boolean);
    return text || `MCP tool "${toolName}" returned an error`;
}
function extractJsonPayload(result, toolName) {
    if (result.structuredContent !== undefined) {
        if (typeof result.structuredContent === "string") {
            return JSON.parse(result.structuredContent);
        }
        return result.structuredContent;
    }
    const textBlocks = extractTextBlocks(result.content).filter((text) => text.trim().length > 0);
    for (const text of textBlocks) {
        try {
            return JSON.parse(text);
        }
        catch {
            continue;
        }
    }
    throw new Error(`MCP tool "${toolName}" returned non-JSON content`);
}
export class MemoryMcpClient {
    dbPath;
    logger;
    client = null;
    transport = null;
    connecting = null;
    availableTools = new Set();
    constructor(dbPath, logger) {
        this.dbPath = dbPath;
        this.logger = logger;
    }
    logInfo(message) {
        this.logger?.info?.(`memory-hybrid-bridge[mcp]: ${message}`);
    }
    logWarn(message) {
        this.logger?.warn?.(`memory-hybrid-bridge[mcp]: ${message}`);
    }
    resolveServerCommand() {
        // Priority: TACHI_BIN > OPENCLAW_MEMORY_SERVER_BIN > local build > PATH (tachi, then memory-server)
        const fromEnv = (process.env.TACHI_BIN || process.env.OPENCLAW_MEMORY_SERVER_BIN)?.trim();
        if (fromEnv) {
            return fromEnv;
        }
        const moduleDir = path.dirname(fileURLToPath(import.meta.url));
        const localBinary = path.resolve(moduleDir, "../../target/release/memory-server");
        if (fs.existsSync(localBinary)) {
            return localBinary;
        }
        // Prefer "tachi" (brew install name) over "memory-server" (dev name)
        return "tachi";
    }
    buildLaunchCandidates() {
        const command = this.resolveServerCommand();
        const env = {
            ...process.env,
            MEMORY_DB_PATH: this.dbPath,
            TACHI_PROFILE: process.env.TACHI_PROFILE || "runtime",
        };
        const candidates = [
            // First candidate: explicit global-db, no project db (clean isolation)
            {
                command,
                args: ["--global-db", this.dbPath, "--no-project-db"],
                env,
                cwd: os.tmpdir(),
            },
            // Second candidate: plain launch — use actual CWD so git root detection works
            {
                command,
                args: [],
                env,
                cwd: process.cwd(),
            },
        ];
        // If primary command is "tachi", also try "memory-server" as last resort
        if (command === "tachi") {
            candidates.push({
                command: "memory-server",
                args: ["--global-db", this.dbPath, "--no-project-db"],
                env,
                cwd: os.tmpdir(),
            });
        }
        return candidates;
    }
    async connectWith(launch) {
        const transport = new StdioClientTransport({
            command: launch.command,
            args: launch.args,
            env: launch.env,
            cwd: launch.cwd,
            stderr: "pipe",
        });
        const client = new Client({
            name: "memory-hybrid-bridge",
            version: "0.0.0",
        }, {});
        await client.connect(transport);
        const listed = await client.listTools();
        const names = new Set(listed.tools.map((tool) => tool.name));
        for (const required of REQUIRED_TOOLS) {
            if (!names.has(required)) {
                await client.close().catch(() => { });
                await transport.close().catch(() => { });
                throw new Error(`required MCP tool missing: ${required}`);
            }
        }
        this.client = client;
        this.transport = transport;
        this.availableTools = names;
        return client;
    }
    async getClient() {
        if (this.client) {
            return this.client;
        }
        if (!this.connecting) {
            this.connecting = (async () => {
                const attempts = this.buildLaunchCandidates();
                let lastError = null;
                for (let i = 0; i < attempts.length; i++) {
                    const launch = attempts[i];
                    try {
                        const client = await this.connectWith(launch);
                        if (i > 0) {
                            this.logWarn("connected via compatibility launch (without --global-db)");
                        }
                        else {
                            this.logInfo(`connected to ${launch.command}`);
                        }
                        return client;
                    }
                    catch (error) {
                        lastError = error;
                        this.client = null;
                        this.transport = null;
                        this.availableTools.clear();
                        continue;
                    }
                }
                throw new Error(`failed to connect memory MCP server: ${String(lastError)}`);
            })().finally(() => {
                this.connecting = null;
            });
        }
        return await this.connecting;
    }
    async resetConnection() {
        const client = this.client;
        const transport = this.transport;
        this.client = null;
        this.transport = null;
        this.availableTools.clear();
        await client?.close().catch(() => { });
        await transport?.close().catch(() => { });
    }
    async close() {
        await this.resetConnection();
    }
    async callJson(name, args = {}) {
        const client = await this.getClient();
        let result;
        try {
            result = (await client.callTool({ name, arguments: args }));
        }
        catch (error) {
            await this.resetConnection();
            throw error;
        }
        if (result.isError) {
            throw new Error(extractErrorMessage(result, name));
        }
        return extractJsonPayload(result, name);
    }
    async saveMemory(entry) {
        await this.callJson("save_memory", {
            id: entry.id,
            text: entry.text,
            summary: entry.summary,
            path: entry.path,
            importance: entry.importance,
            category: entry.category,
            topic: entry.topic,
            keywords: entry.keywords,
            persons: entry.persons,
            entities: entry.entities,
            location: entry.location,
            scope: entry.scope,
            vector: entry.vector,
            force: true,
            auto_link: false,
        });
    }
    async getMemory(id) {
        const payload = await this.callJson("get_memory", {
            id,
            include_archived: false,
        });
        if (isRecord(payload) && typeof payload.error === "string") {
            return undefined;
        }
        return coerceMemoryEntry(payload);
    }
    async listMemories(limit) {
        const payload = await this.callJson("list_memories", {
            path_prefix: "/",
            limit,
            include_archived: false,
        });
        if (!Array.isArray(payload)) {
            return [];
        }
        return payload.map((row) => coerceMemoryEntry(row)).filter((entry) => Boolean(entry));
    }
    async searchMemory(query, queryVec, opts) {
        const payload = await this.callJson("search_memory", {
            query,
            top_k: opts?.top_k,
            path_prefix: opts?.path_prefix,
            include_archived: false,
            candidates_per_channel: opts?.candidates,
            graph_expand_hops: 0,
            graph_relation_filter: null,
            ...(queryVec && queryVec.length > 0 ? { query_vec: queryVec } : {}),
            ...(opts?.weights ? { weights: opts.weights } : {}),
        });
        const docs = [];
        const scores = {};
        const scoreBreakdowns = {};
        if (!Array.isArray(payload)) {
            return { docs, scores, scoreBreakdowns };
        }
        for (const row of payload) {
            const entry = coerceMemoryEntry(row);
            if (!entry) {
                continue;
            }
            const scoreRecord = isRecord(row) && isRecord(row.score) ? row.score : null;
            const finalScore = asFiniteNumber(scoreRecord?.final ?? scoreRecord?.final_score ?? (isRecord(row) ? row.relevance : undefined));
            const breakdown = {
                vector: asFiniteNumber(scoreRecord?.vector),
                fts: asFiniteNumber(scoreRecord?.fts),
                symbolic: asFiniteNumber(scoreRecord?.symbolic),
                decay: asFiniteNumber(scoreRecord?.decay),
                final: finalScore,
            };
            docs.push(entry);
            scores[entry.id] = finalScore;
            scoreBreakdowns[entry.id] = breakdown;
        }
        return { docs, scores, scoreBreakdowns };
    }
    async recallContext(query, opts) {
        const payload = await this.callJson("recall_context", {
            query,
            top_k: opts?.top_k,
            candidate_multiplier: opts?.candidate_multiplier,
            path_prefix: opts?.path_prefix,
            agent_id: opts?.agent_id,
            exclude_topics: opts?.exclude_topics,
            min_score: opts?.min_score,
        });
        let prependContext = "";
        const results = [];
        if (isRecord(payload) && typeof payload.prepend_context === "string") {
            prependContext = payload.prepend_context;
        }
        const rows = isRecord(payload) && Array.isArray(payload.results) ? payload.results : [];
        for (const row of rows) {
            const entry = coerceMemoryEntry(row);
            if (!entry) {
                continue;
            }
            const finalScore = asFiniteNumber((isRecord(row) ? row.relevance : undefined) ??
                (isRecord(row) && isRecord(row.score) ? row.score.final : undefined));
            results.push({ entry, final_score: finalScore });
        }
        return { prependContext, results };
    }
    async captureSession(params) {
        return await this.callJson("capture_session", params);
    }
    async compactContext(params) {
        return await this.callJson("compact_context", params);
    }
    async findSimilarMemory(queryVec, topK) {
        if (!this.availableTools.has("find_similar_memory")) {
            throw new Error("find_similar_memory tool is unavailable");
        }
        const payload = await this.callJson("find_similar_memory", {
            query_vec: queryVec,
            top_k: topK,
            candidates_per_channel: Math.max(topK, 20),
            include_archived: false,
        });
        if (!Array.isArray(payload)) {
            return [];
        }
        const out = [];
        for (const row of payload) {
            const entry = coerceMemoryEntry(row);
            if (!entry) {
                continue;
            }
            const similarity = asFiniteNumber((isRecord(row) ? row.similarity : undefined) ??
                (isRecord(row) && isRecord(row.score) ? row.score.vector : undefined));
            if (similarity > 0) {
                out.push({ entry, similarity });
            }
        }
        return out;
    }
    async deleteMemory(id) {
        const payload = await this.callJson("delete_memory", { id });
        if (!isRecord(payload)) {
            return false;
        }
        return payload.deleted === true;
    }
    async memoryStats() {
        return await this.callJson("memory_stats", {});
    }
    async callTool(toolName, args) {
        return await this.callJson(toolName, args);
    }
}
