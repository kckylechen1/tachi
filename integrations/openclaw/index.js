import path from "node:path";
import { Type } from "@sinclair/typebox";
import { bridgeConfigSchema } from "./config.js";
import { MemoryMcpClient } from "./mcp-client.js";
function resolveConfigPath(api, configuredPath) {
    return path.isAbsolute(configuredPath) ? configuredPath : api.resolvePath(configuredPath);
}
function textResult(text, details) {
    return {
        content: [{ type: "text", text }],
        ...(details ? { details } : {}),
    };
}
function makeMemoryId() {
    return `m_${Date.now()}_${Math.random().toString(16).slice(2, 10)}`;
}
function messageToText(message) {
    if (!message) {
        return "";
    }
    if (typeof message.content === "string") {
        return message.content;
    }
    if (Array.isArray(message.content)) {
        return message.content
            .map((block) => {
            if (typeof block === "string") {
                return block;
            }
            if (block && typeof block.text === "string") {
                return block.text;
            }
            return "";
        })
            .filter(Boolean)
            .join("\n");
    }
    return "";
}
// ============================================================================
// Plugin Definition
// ============================================================================
export const memoryHybridBridgePlugin = {
    id: "tachi",
    name: "Memory Hybrid Bridge",
    kind: "memory",
    description: "Advanced structured memory with LLM extraction and hybrid retrieval (vector/lexical/symbolic)",
    register(api) {
        const config = bridgeConfigSchema.parse(api.pluginConfig);
        const dbPath = resolveConfigPath(api, config.dbPath);
        let initClient = null;
        function ensureClient() {
            if (!initClient) {
                initClient = Promise.resolve(new MemoryMcpClient(dbPath, api.logger));
            }
            return initClient;
        }
        async function runWithClient(operation, run) {
            try {
                const client = await ensureClient();
                return { ok: true, value: await run(client) };
            }
            catch (error) {
                api.logger.warn(`tachi: ${operation} unavailable: ${String(error)}`);
                return { ok: false, error };
            }
        }
        async function performSearch(query, searchTopK) {
            const topK = searchTopK ?? config.topK;
            const result = await runWithClient("search_memory", async (client) => {
                const { docs, scores } = await client.searchMemory(query, undefined, {
                    top_k: topK,
                    weights: config.weights,
                });
                return docs.map((entry) => ({
                    final_score: scores[entry.id] ?? 0,
                    entry,
                }));
            });
            if (!result.ok) {
                return { available: false, message: "Tachi MCP client unavailable." };
            }
            return { available: true, hits: result.value };
        }
        async function performRecall(query, agentId) {
            const result = await runWithClient("recall_context", async (client) => await client.recallContext(query, {
                top_k: config.topK,
                candidate_multiplier: 1,
                agent_id: agentId,
                exclude_topics: ["imsg_conversation"],
            }));
            return result.ok ? result.value : null;
        }
        function formatSearchResults(result) {
            if (!result.available) {
                return textResult(result.message, {
                    available: false,
                    count: 0,
                    results: [],
                });
            }
            if (result.hits.length === 0) {
                return textResult("No relevant memories found.", {
                    available: true,
                    count: 0,
                    results: [],
                });
            }
            const results = result.hits.map((hit) => {
                const entry = hit.entry;
                return {
                    path: `memory/${entry.id}`,
                    startLine: 1,
                    endLine: 1,
                    score: hit.final_score,
                    snippet: [
                        `[${entry.topic}] ${entry.text}`,
                        `Keywords: ${entry.keywords.join(", ")}`,
                        `Persons: ${entry.persons.join(", ")}`,
                        entry.entities.length ? `Entities: ${entry.entities.join(", ")}` : "",
                        `Timestamp: ${entry.timestamp}`,
                    ]
                        .filter(Boolean)
                        .join("\n"),
                };
            });
            return textResult(JSON.stringify({ results }), {
                available: true,
                count: result.hits.length,
                results,
            });
        }
        api.logger.info("tachi: registered (MCP compatibility mode)");
        // ========================================================================
        // Tools — compatibility wrappers that forward directly to Tachi MCP.
        // ========================================================================
        api.registerTool({
            name: "memory_search",
            label: "Memory Search",
            description: "Mandatory recall step: semantically search long-term structured memory before answering questions about prior work, decisions, dates, people, preferences, or todos; returns top snippets with relevance scores.",
            parameters: Type.Object({
                query: Type.String({ description: "Natural language search query" }),
                maxResults: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
                minScore: Type.Optional(Type.Number({ description: "Min score threshold (default: 0)" })),
            }),
            async execute(_toolCallId, params) {
                const { query, maxResults, minScore } = params;
                const result = await performSearch(query, maxResults ?? config.topK);
                if (!result.available) {
                    return formatSearchResults(result);
                }
                const hits = typeof minScore === "number" && minScore > 0
                    ? result.hits.filter((hit) => hit.final_score >= minScore)
                    : result.hits;
                return formatSearchResults({ available: true, hits });
            },
        });
        api.registerTool({
            name: "memory_get",
            label: "Memory Get",
            description: "Retrieve a specific memory entry by id; use after memory_search to get full details.",
            parameters: Type.Object({
                path: Type.String({
                    description: "Entry id (e.g. memory/m_1234) or raw id (m_1234)",
                }),
                from: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
                lines: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
            }),
            async execute(_toolCallId, params) {
                const rawPath = params.path;
                const entryId = rawPath.replace(/^(?:shadow-store|memory)\//, "");
                const result = await runWithClient("get_memory", async (client) => await client.getMemory(entryId));
                if (!result.ok) {
                    return textResult("Tachi MCP client unavailable.", {
                        available: false,
                        found: false,
                    });
                }
                const found = result.value;
                if (!found) {
                    return textResult(JSON.stringify({
                        path: rawPath,
                        text: "",
                        error: `Memory entry not found: ${entryId}`,
                    }), { available: true, found: false });
                }
                const text = [
                    `ID: ${found.id}`,
                    `Topic: ${found.topic}`,
                    `Timestamp: ${found.timestamp}`,
                    `Fact: ${found.text}`,
                    `Keywords: ${found.keywords.join(", ")}`,
                    `Persons: ${found.persons.join(", ")}`,
                    `Entities: ${found.entities.join(", ")}`,
                ].join("\n");
                return textResult(JSON.stringify({ path: rawPath, text }), {
                    available: true,
                    found: true,
                });
            },
        });
        api.registerTool({
            name: "memory_save",
            label: "Memory Save",
            description: "Save a durable memory into Tachi for future recall.",
            parameters: Type.Object({
                text: Type.String({ description: "Memory text content" }),
                summary: Type.Optional(Type.String({ description: "Optional short summary" })),
                topic: Type.Optional(Type.String({ description: "Memory topic" })),
                path: Type.Optional(Type.String({ description: "Hierarchical path (defaults to agent root)" })),
                importance: Type.Optional(Type.Number({ description: "0.0-1.0 importance score" })),
                keywords: Type.Optional(Type.Array(Type.String(), { description: "Keyword tags" })),
                category: Type.Optional(Type.String({ description: "Category: fact | decision | preference | entity | other" })),
            }),
            async execute(_toolCallId, params, _signal, context) {
                const { text, summary, topic, path: memoryPath, importance, keywords, category } = params;
                const agentId = context?.agentId || "main";
                const result = await runWithClient("save_memory", async (client) => {
                    await client.saveMemory({
                        id: makeMemoryId(),
                        text,
                        summary: summary ?? text.slice(0, 96),
                        path: memoryPath || `/openclaw/agent-${agentId}`,
                        importance: importance ?? 0.7,
                        category: (category || "fact"),
                        topic: topic || "manual_memory",
                        keywords: keywords || [],
                        persons: [],
                        entities: [],
                        location: "",
                        timestamp: new Date().toISOString(),
                        scope: "project",
                        access_count: 0,
                        last_access: null,
                        metadata: { source_refs: [] },
                    });
                    return { ok: true };
                });
                return result.ok
                    ? textResult(JSON.stringify(result.value))
                    : textResult("Tachi MCP client unavailable.");
            },
        });
        api.registerTool({
            name: "memory_graph",
            label: "Memory Graph",
            description: "Inspect a read-only neighborhood in Tachi's memory graph by memory id or query.",
            parameters: Type.Object({
                memory_id: Type.Optional(Type.String({ description: "Seed memory id" })),
                query: Type.Optional(Type.String({ description: "Natural language graph lookup query" })),
                top_k: Type.Optional(Type.Number({ description: "Query seed count (default: 5)" })),
                depth: Type.Optional(Type.Number({ description: "Traversal depth (default: 1)" })),
            }),
            async execute(_toolCallId, params) {
                const { memory_id, query, top_k, depth } = params;
                const result = await runWithClient("memory_graph", async (client) => await client.memoryGraph({
                    memory_id,
                    query,
                    top_k,
                    depth,
                }));
                return result.ok
                    ? textResult(JSON.stringify(result.value))
                    : textResult("Tachi MCP client unavailable.");
            },
        });
        api.on("before_agent_start", async (event, context) => {
            const query = event.prompt;
            if (!query || query.length < 5) {
                return;
            }
            const agentId = context?.agentId || "main";
            const recall = await performRecall(query, agentId);
            if (recall?.prependContext.trim()) {
                return { prependContext: recall.prependContext };
            }
        });
        api.on("agent_end", async (event, context) => {
            if (!event?.success || !Array.isArray(event?.messages) || event.messages.length === 0) {
                return;
            }
            const recentMessages = event.messages
                .slice(-8)
                .map((message) => ({
                role: typeof message?.role === "string" ? message.role : "unknown",
                content: messageToText(message),
            }))
                .filter((message) => message.content.trim().length > 0);
            const combinedChars = recentMessages.reduce((total, message) => total + message.content.length, 0);
            if (recentMessages.length === 0 || combinedChars < config.captureMinChars) {
                return;
            }
            const agentId = context?.agentId || "main";
            const conversationId = context?.sessionKey || event?.conversationId || event?.sessionId || `openclaw:${agentId}`;
            const turnId = event?.turnId || event?.runId || `agent_end:${Date.now()}`;
            const result = await runWithClient("capture_session", async (client) => await client.captureSession({
                conversation_id: conversationId,
                turn_id: turnId,
                agent_id: agentId,
                messages: recentMessages,
                path_prefix: `/openclaw/agent-${agentId}`,
                scope: "project",
            }));
            if (!result.ok) {
                api.logger.warn("tachi: capture_session skipped in degraded mode");
            }
        });
        api.registerService({
            id: "tachi",
            start: () => api.logger.info("tachi: service started"),
            stop: async () => {
                api.logger.info("tachi: shutting down...");
                const clientPromise = initClient;
                initClient = null;
                if (clientPromise) {
                    try {
                        const client = await clientPromise;
                        await client.close();
                    }
                    catch {
                        // Client never initialized successfully.
                    }
                }
                api.logger.info("tachi: service stopped");
            },
        });
    },
};
export default memoryHybridBridgePlugin;
