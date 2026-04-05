import { createHash } from "node:crypto";
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
function normalizeBracketInsight(text) {
    return String(text || "").replace(/\s+/g, " ").trim();
}
function stripCoreRuleAnchor(text) {
    return normalizeBracketInsight(text).replace(/^\[核心法则\]\s*/i, "").trim();
}
function summarizeInsight(text) {
    const normalized = stripCoreRuleAnchor(text);
    return normalized.length <= 28 ? normalized : `${normalized.slice(0, 27)}…`;
}
function isSelfEvolutionInsight(text) {
    const normalized = normalizeBracketInsight(text);
    if (!normalized) {
        return false;
    }
    if (normalized.includes("[核心法则]")) {
        return true;
    }
    return /(原来.{0,20}(喜欢|不喜欢)|记住了|下次我要|下次我会|以后我要|以后我会|雷区|更吃这一套|不吃这一套|这样更有效|这种方式有用|策略失败|无效)/i.test(normalized);
}
function classifySelfEvolution(text) {
    if (/(记住了|下次我要|下次我会|以后我要|以后我会|策略失败|无效)/i.test(text)) {
        return "decision";
    }
    if (/(喜欢|不喜欢|雷区|偏好|讨厌|更吃|不吃)/i.test(text)) {
        return "preference";
    }
    return "other";
}
function buildSelfEvolutionId(agentId, note) {
    const seed = `${agentId.trim()}:${stripCoreRuleAnchor(note)}`;
    return `self-evo-${createHash("sha1").update(seed).digest("hex").slice(0, 16)}`;
}
function extractSelfEvolutionInsights(messages) {
    const insights = [];
    const seen = new Set();
    for (let messageIndex = 0; messageIndex < messages.length; messageIndex++) {
        const message = messages[messageIndex];
        if (message?.role !== "assistant") {
            continue;
        }
        const text = messageToText(message);
        const matches = text.matchAll(/[（(]([^()（）\n]{4,240})[)）]/g);
        for (const match of matches) {
            const raw = normalizeBracketInsight(match[1]);
            const note = stripCoreRuleAnchor(raw);
            const anchored = raw.includes("[核心法则]");
            if (!note || note.length < 4 || !isSelfEvolutionInsight(raw)) {
                continue;
            }
            const dedupeKey = note.toLowerCase();
            if (seen.has(dedupeKey)) {
                continue;
            }
            seen.add(dedupeKey);
            insights.push({ note, messageIndex, anchored });
        }
    }
    return insights;
}
function buildSelfEvolutionMemory(agentId, sessionKey, insight, insightIndex, timestamp) {
    const category = classifySelfEvolution(insight.note);
    const isJayne = agentId === "jayne";
    return {
        id: buildSelfEvolutionId(agentId, insight.note),
        text: insight.note,
        summary: summarizeInsight(insight.note),
        keywords: [
            agentId,
            "self-evolution",
            "bracket-note",
            insight.anchored ? "core-rule" : "",
            category === "decision" ? "strategy" : "",
            category === "preference" && isJayne ? "kyle-preference" : "",
        ].filter(Boolean),
        timestamp,
        location: "agent_end",
        persons: isJayne ? ["Kyle"] : [],
        entities: category === "preference" && isJayne ? ["Kyle preference"] : [],
        topic: isJayne ? "jayne_self_evolution" : "agent_self_evolution",
        scope: "project",
        path: `/openclaw/agent-${agentId}/self-evolution`,
        category,
        importance: insight.anchored ? 0.92 : 0.88,
        access_count: 0,
        last_access: null,
        metadata: {
            source_refs: [
                {
                    ref_type: "message",
                    ref_id: `${sessionKey}:assistant:${insight.messageIndex}`,
                },
            ],
            bracket_note: true,
            self_evolution: true,
            extracted_by: insight.anchored ? "agent_end_anchor_capture" : "agent_end_bracket_capture",
            insight_index: insightIndex,
        },
    };
}
function hasCaptureTrigger(messages, keywords) {
    if (keywords.length === 0) {
        return false;
    }
    const haystack = messages.map((message) => message.content).join("\n").toLowerCase();
    return keywords.some((keyword) => keyword.trim() && haystack.includes(keyword.toLowerCase()));
}
function formatJsonTextResult(value) {
    return textResult(JSON.stringify(value, null, 2));
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
        const configuredDbPath = resolveConfigPath(api, config.dbPath);
        const clientCache = new Map();
        function resolveAgentDbPath(agentId) {
            const normalizedAgentId = (agentId || "main").trim() || "main";
            const baseDir = path.dirname(configuredDbPath);
            const dbName = path.basename(configuredDbPath) || "memory.db";
            return path.resolve(baseDir, `agents/${normalizedAgentId}/${dbName}`);
        }
        function ensureClient(agentId) {
            const dbPath = resolveAgentDbPath(agentId);
            let initClient = clientCache.get(dbPath);
            if (!initClient) {
                initClient = Promise.resolve(new MemoryMcpClient(dbPath, api.logger));
                clientCache.set(dbPath, initClient);
            }
            return initClient;
        }
        async function runWithClient(operation, run, agentId) {
            try {
                const client = await ensureClient(agentId);
                return { ok: true, value: await run(client) };
            }
            catch (error) {
                api.logger.warn(`tachi: ${operation} unavailable: ${String(error)}`);
                return { ok: false, error };
            }
        }
        async function performSearch(query, searchTopK, agentId) {
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
            }, agentId);
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
            }), agentId);
            return result.ok ? result.value : null;
        }
        function registerTachiPassthrough(openClawToolName, tachiToolName, description) {
            api.registerTool({
                name: openClawToolName,
                label: openClawToolName,
                description,
                parameters: Type.Object({}, {
                    additionalProperties: true,
                    description: "Arguments forwarded directly to the underlying Tachi MCP tool.",
                }),
                async execute(_toolCallId, params, _signal, context) {
                    const agentId = context?.agentId || "main";
                    const result = await runWithClient(tachiToolName, async (client) => await client.callTool(tachiToolName, params || {}), agentId);
                    return result.ok
                        ? formatJsonTextResult(result.value)
                        : textResult("Tachi MCP client unavailable.");
                },
            });
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
            async execute(_toolCallId, params, _signal, context) {
                const { query, maxResults, minScore } = params;
                const agentId = context?.agentId || "main";
                const result = await performSearch(query, maxResults ?? config.topK, agentId);
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
            async execute(_toolCallId, params, _signal, context) {
                const rawPath = params.path;
                const entryId = rawPath.replace(/^(?:shadow-store|memory)\//, "");
                const agentId = context?.agentId || "main";
                const result = await runWithClient("get_memory", async (client) => await client.getMemory(entryId), agentId);
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
                }, agentId);
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
            async execute(_toolCallId, params, _signal, context) {
                const { memory_id, query, top_k, depth } = params;
                const agentId = context?.agentId || "main";
                const result = await runWithClient("memory_graph", async (client) => await client.memoryGraph({
                    memory_id,
                    query,
                    top_k,
                    depth,
                }), agentId);
                return result.ok
                    ? textResult(JSON.stringify(result.value))
                    : textResult("Tachi MCP client unavailable.");
            },
        });
        api.registerTool({
            name: "memory_delete",
            label: "Memory Delete",
            description: "Delete a specific memory entry by id from Tachi.",
            parameters: Type.Object({
                path: Type.String({
                    description: "Entry id (e.g. memory/m_1234) or raw id (m_1234)",
                }),
            }),
            async execute(_toolCallId, params, _signal, context) {
                const rawPath = params.path;
                const entryId = rawPath.replace(/^(?:shadow-store|memory)\//, "");
                const agentId = context?.agentId || "main";
                const result = await runWithClient("delete_memory", async (client) => await client.deleteMemory(entryId), agentId);
                return result.ok
                    ? formatJsonTextResult({ deleted: result.value, id: entryId })
                    : textResult("Tachi MCP client unavailable.");
            },
        });
        api.registerTool({
            name: "compact_context",
            label: "Compact Context",
            description: "Compact the current session window via Tachi MCP and return a reusable summary block.",
            parameters: Type.Object({
                conversation_id: Type.String({ description: "Conversation identifier" }),
                window_id: Type.String({ description: "Compaction window identifier" }),
                messages: Type.Array(Type.Object({
                    role: Type.String(),
                    content: Type.String(),
                }), { description: "Recent messages to compact" }),
                trigger: Type.Optional(Type.String()),
                current_summary: Type.Optional(Type.String()),
                path_prefix: Type.Optional(Type.String()),
                target_tokens: Type.Optional(Type.Number()),
                max_output_tokens: Type.Optional(Type.Number()),
                persist: Type.Optional(Type.Boolean()),
            }),
            async execute(_toolCallId, params, _signal, context) {
                const agentId = context?.agentId || "main";
                const payload = params;
                const result = await runWithClient("compact_context", async (client) => await client.compactContext({
                    agent_id: agentId,
                    conversation_id: payload.conversation_id,
                    window_id: payload.window_id,
                    messages: payload.messages,
                    trigger: payload.trigger,
                    current_summary: payload.current_summary,
                    path_prefix: payload.path_prefix,
                    target_tokens: payload.target_tokens,
                    max_output_tokens: payload.max_output_tokens,
                    persist: payload.persist,
                }), agentId);
                return result.ok
                    ? formatJsonTextResult(result.value)
                    : textResult("Tachi MCP client unavailable.");
            },
        });
        registerTachiPassthrough("tachi_vault_store", "vault_set", "Store a secret in the Tachi vault.");
        registerTachiPassthrough("tachi_vault_retrieve", "vault_get", "Retrieve a secret from the Tachi vault.");
        registerTachiPassthrough("tachi_vault_list", "vault_list", "List secrets in the Tachi vault.");
        registerTachiPassthrough("tachi_ghost_whisper", "ghost_publish", "Publish a Ghost whisper message.");
        registerTachiPassthrough("tachi_ghost_listen", "ghost_subscribe", "Listen for Ghost whisper messages.");
        registerTachiPassthrough("tachi_kanban_add", "post_card", "Create a kanban card in Tachi.");
        registerTachiPassthrough("tachi_kanban_update", "update_card", "Update a kanban card in Tachi.");
        registerTachiPassthrough("tachi_kanban_list", "check_inbox", "List kanban cards from a Tachi inbox.");
        registerTachiPassthrough("tachi_create_handoff", "handoff_leave", "Create a Tachi handoff memo.");
        registerTachiPassthrough("tachi_get_handoff", "handoff_check", "Read pending Tachi handoff memos.");
        registerTachiPassthrough("tachi_run_skill", "run_skill", "Run a Tachi skill.");
        registerTachiPassthrough("tachi_hub_discover", "hub_discover", "Discover available Tachi hub capabilities.");
        registerTachiPassthrough("tachi_recommend_toolchain", "recommend_toolchain", "Recommend a Tachi toolchain for the current task.");
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
            const agentId = context?.agentId || "main";
            const conversationId = context?.sessionKey || event?.conversationId || event?.sessionId || `openclaw:${agentId}`;
            const turnId = event?.turnId || event?.runId || `agent_end:${Date.now()}`;
            const selfEvolutionAgents = new Set(config.selfEvolutionAgents.map((value) => value.toLowerCase()));
            if (selfEvolutionAgents.has(agentId.toLowerCase())) {
                const insights = extractSelfEvolutionInsights(event.messages);
                if (insights.length > 0) {
                    let saved = 0;
                    for (const [insightIndex, insight] of insights.entries()) {
                        const result = await runWithClient("save_memory", async (client) => {
                            await client.saveMemory(buildSelfEvolutionMemory(agentId, conversationId, insight, insightIndex, new Date().toISOString()));
                            return { ok: true };
                        }, agentId);
                        if (result.ok) {
                            saved += 1;
                        }
                    }
                    if (saved > 0) {
                        api.logger.info(`tachi: saved ${saved} self-evolution notes for ${agentId}`);
                    }
                }
            }
            const recentMessages = event.messages
                .slice(-8)
                .map((message) => ({
                role: typeof message?.role === "string" ? message.role : "unknown",
                content: messageToText(message),
            }))
                .filter((message) => message.content.trim().length > 0);
            const combinedChars = recentMessages.reduce((total, message) => total + message.content.length, 0);
            const hasKeywordTrigger = hasCaptureTrigger(recentMessages, config.captureTriggerKeywords);
            if (recentMessages.length === 0 || (combinedChars < config.captureMinChars && !hasKeywordTrigger)) {
                return;
            }
            const result = await runWithClient("capture_session", async (client) => await client.captureSession({
                conversation_id: conversationId,
                turn_id: turnId,
                agent_id: agentId,
                messages: recentMessages,
                path_prefix: `/openclaw/agent-${agentId}`,
                scope: "project",
            }), agentId);
            if (!result.ok) {
                api.logger.warn("tachi: capture_session skipped in degraded mode");
            }
        });
        api.registerService({
            id: "tachi",
            start: () => api.logger.info("tachi: service started"),
            stop: async () => {
                api.logger.info("tachi: shutting down...");
                const clientPromises = Array.from(clientCache.values());
                clientCache.clear();
                for (const clientPromise of clientPromises) {
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
