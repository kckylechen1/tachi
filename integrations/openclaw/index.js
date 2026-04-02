import fs from "node:fs/promises";
import path from "node:path";
import { Type } from "@sinclair/typebox";
import { bridgeConfigSchema, pluginDataDir, workspaceRoot } from "./config.js";
import { getStore } from "./store.js";
// ============================================================================
// Internal Helpers
// ============================================================================
async function ensureFile(filePath) {
    await fs.mkdir(path.dirname(filePath), { recursive: true });
    try {
        await fs.access(filePath);
    }
    catch {
        await fs.writeFile(filePath, "", "utf8");
    }
}
async function appendAuditLog(auditLogPath, payload) {
    await ensureFile(auditLogPath);
    const line = { ts: new Date().toISOString(), ...payload };
    await fs.appendFile(auditLogPath, `${JSON.stringify(line)}\n`, "utf8");
}
function getCaptureSpoolPath(auditLogPath) {
    return path.resolve(path.dirname(auditLogPath), "capture-spool.jsonl");
}
async function readCaptureSpool(spoolPath) {
    try {
        const text = await fs.readFile(spoolPath, "utf8");
        return text
            .split("\n")
            .map((line) => line.trim())
            .filter(Boolean)
            .map((line) => JSON.parse(line));
    }
    catch {
        return [];
    }
}
async function writeCaptureSpool(spoolPath, payloads) {
    if (payloads.length === 0) {
        try {
            await fs.unlink(spoolPath);
        }
        catch {
            // nothing to clean up
        }
        return;
    }
    await ensureFile(spoolPath);
    const body = `${payloads.map((payload) => JSON.stringify(payload)).join("\n")}\n`;
    await fs.writeFile(spoolPath, body, "utf8");
}
function capturePayloadKey(payload) {
    return `${payload.agent_id}:${payload.conversation_id}:${payload.turn_id}`;
}
async function enqueueCapturePayload(spoolPath, payload) {
    const pending = await readCaptureSpool(spoolPath);
    const key = capturePayloadKey(payload);
    if (!pending.some((item) => capturePayloadKey(item) === key)) {
        pending.push(payload);
        await writeCaptureSpool(spoolPath, pending);
    }
    return pending.length;
}
async function flushCaptureSpool(spoolPath, client, logger, agentId) {
    const pending = await readCaptureSpool(spoolPath);
    if (pending.length === 0) {
        return { replayed: 0, remaining: 0 };
    }
    const remaining = [];
    let replayed = 0;
    for (let index = 0; index < pending.length; index += 1) {
        const payload = pending[index];
        try {
            const result = await client.captureSession(payload);
            const status = typeof result?.status === "string" ? result.status : "error";
            if (status === "completed" || status === "skipped") {
                replayed += 1;
                continue;
            }
            remaining.push(payload);
        }
        catch (err) {
            logger.warn?.(`tachi [${agentId}]: replay capture spool failed: ${String(err)}`);
            remaining.push(payload, ...pending.slice(index + 1));
            break;
        }
    }
    await writeCaptureSpool(spoolPath, remaining);
    return { replayed, remaining: remaining.length };
}
function resolveConfigPath(api, configuredPath) {
    return path.isAbsolute(configuredPath) ? configuredPath : api.resolvePath(configuredPath);
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
        const initStores = new Map();
        // Circuit breaker for auto-capture extraction
        let extractFailCount = 0;
        let extractBackoffUntil = 0;
        const EXTRACT_FAIL_THRESHOLD = 3;
        const EXTRACT_BACKOFF_MS = 5 * 60 * 1000; // 5 minutes
        function markCaptureHealthy() {
            extractFailCount = 0;
        }
        function noteCaptureFailure(agentId, error) {
            extractFailCount += 1;
            if (extractFailCount >= EXTRACT_FAIL_THRESHOLD) {
                extractBackoffUntil = Date.now() + EXTRACT_BACKOFF_MS;
                api.logger.warn(`tachi [${agentId}]: auto-capture circuit breaker OPEN — ${extractFailCount} consecutive failures, backing off ${EXTRACT_BACKOFF_MS / 1000}s`);
                extractFailCount = 0;
                return;
            }
            api.logger.warn(`tachi [${agentId}]: auto-capture failed (${extractFailCount}/${EXTRACT_FAIL_THRESHOLD}): ${String(error)}`);
        }
        function getResolvedPaths(agentId) {
            // main and ops share the default store; other agents get scoped paths
            const id = agentId || "main";
            if (id === "main" || id === "ops") {
                return {
                    db: resolveConfigPath(api, config.dbPath),
                    shadow: resolveConfigPath(api, config.shadowStorePath),
                    audit: resolveConfigPath(api, config.auditLogPath),
                };
            }
            // Never fall back to process.cwd(); some OpenClaw launches end up at "/",
            // which would redirect writes into /agents or /data and fail on normal setups.
            const agentMemDir = workspaceRoot
                ? path.resolve(workspaceRoot, "agents", id, "memory")
                : path.resolve(pluginDataDir, "agents", id);
            return {
                db: path.resolve(agentMemDir, "memory.db"),
                shadow: path.resolve(agentMemDir, "shadow-store.jsonl"),
                audit: path.resolve(agentMemDir, "audit-log.jsonl"),
            };
        }
        async function ensureStore(agentId) {
            const paths = getResolvedPaths(agentId);
            if (!initStores.has(paths.db)) {
                // C3 fix: don't cache rejected promises — delete on failure so next call retries
                const promise = getStore(paths.db, paths.shadow, api.logger).catch((err) => {
                    initStores.delete(paths.db);
                    throw err;
                });
                initStores.set(paths.db, promise);
            }
            return await initStores.get(paths.db);
        }
        api.logger.info(`tachi: registered (dynamic agent-scoping enabled)`);
        // --- Search Logic ---
        // User-initiated search should prefer Tachi server-side recall. Local fallback
        // stays FTS-only so the plugin does not keep a parallel model pipeline.
        async function performSearch(query, agentId, searchTopK) {
            const topK = searchTopK ?? config.topK;
            const store = await ensureStore(agentId);
            const client = store.getMcpClient();
            if (client) {
                try {
                    const recall = await client.recallContext(query, {
                        top_k: topK,
                        candidate_multiplier: 3,
                        agent_id: agentId,
                    });
                    return recall.results;
                }
                catch (err) {
                    api.logger.warn(`tachi: recall_context failed, fallback to local FTS search: ${String(err)}`);
                }
            }
            return await performFtsSearch(query, agentId, topK);
        }
        // FTS-only search — zero network calls, used for automatic context injection.
        // Used as the only local resilience path while Tachi owns the main recall logic.
        async function performFtsSearch(query, agentId, searchTopK) {
            const store = await ensureStore(agentId);
            const topK = searchTopK ?? config.topK;
            const { docs, scores } = await store.search(query, undefined, {
                top_k: topK,
                weights: config.weights
            });
            return docs.map((doc) => ({ final_score: scores[doc.id] ?? 0, entry: doc }));
        }
        // ========================================================================
        // Tools — register as memory_search / memory_get so the agent's
        // natural tool calls hit the hybrid shadow store directly.
        // Requires plugins.slots.memory = "tachi" in openclaw.json.
        // ========================================================================
        function formatSearchResults(hits) {
            if (hits.length === 0) {
                return {
                    content: [{ type: "text", text: "No relevant memories found." }],
                    details: { count: 0, results: [] },
                };
            }
            const results = hits.map((h) => {
                const e = h.entry;
                return {
                    path: `shadow-store/${e.id}`,
                    startLine: 1,
                    endLine: 1,
                    score: h.final_score,
                    snippet: [
                        `[${e.topic}] ${e.text}`,
                        `Keywords: ${e.keywords.join(", ")}`,
                        `Persons: ${e.persons.join(", ")}`,
                        e.entities.length ? `Entities: ${e.entities.join(", ")}` : "",
                        `Timestamp: ${e.timestamp}`,
                    ]
                        .filter(Boolean)
                        .join("\n"),
                };
            });
            return {
                content: [{ type: "text", text: JSON.stringify({ results }) }],
                details: { count: hits.length, results },
            };
        }
        api.registerTool({
            name: "memory_search",
            label: "Memory Search",
            description: "Mandatory recall step: semantically search long-term structured memory before answering questions about prior work, decisions, dates, people, preferences, or todos; returns top snippets with relevance scores.",
            parameters: Type.Object({
                query: Type.String({ description: "Natural language search query" }),
                maxResults: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
                minScore: Type.Optional(Type.Number({ description: "Min score threshold (default: 0)" })),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const { query, maxResults, minScore } = params;
                const agentId = _context?.agentId || "main";
                const topK = maxResults ?? config.topK;
                let hits = await performSearch(query, agentId, topK);
                if (typeof minScore === "number" && minScore > 0) {
                    hits = hits.filter((h) => h.final_score >= minScore);
                }
                return formatSearchResults(hits);
            },
        });
        api.registerTool({
            name: "memory_hybrid_search",
            label: "Memory Hybrid Search",
            description: "Search long-term structured memory using vector, lexical, and symbolic hybrid scoring.",
            parameters: Type.Object({
                query: Type.String({ description: "Natural language search query" }),
                top_k: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const { query, top_k } = params;
                const agentId = _context?.agentId || "main";
                const hits = await performSearch(query, agentId, top_k);
                return formatSearchResults(hits);
            },
        });
        api.registerTool({
            name: "memory_get",
            label: "Memory Get",
            description: "Retrieve a specific memory entry by id from the shadow store; use after memory_search to get full details.",
            parameters: Type.Object({
                path: Type.String({
                    description: "Entry id (e.g. shadow-store/m_1234) or raw id (m_1234)",
                }),
                from: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
                lines: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const rawPath = params.path;
                const entryId = rawPath.replace(/^shadow-store\//, "");
                const agentId = _context?.agentId || "main";
                const store = await ensureStore(agentId);
                const found = await store.get(entryId);
                if (!found) {
                    return {
                        content: [
                            {
                                type: "text",
                                text: JSON.stringify({
                                    path: rawPath,
                                    text: "",
                                    error: `Memory entry not found: ${entryId}`,
                                }),
                            },
                        ],
                        details: { found: false },
                    };
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
                return {
                    content: [{ type: "text", text: JSON.stringify({ path: rawPath, text }) }],
                    details: { found: true },
                };
            },
        });
        // ── Tachi Passthrough Tools ──────────────────────────────────
        api.registerTool({
            name: "tachi_ghost_publish",
            label: "Ghost Whisper",
            description: "Publish a message to a Ghost Whispers topic for inter-agent communication.",
            parameters: Type.Object({
                topic: Type.String({ description: "Topic name (e.g. 'build-status', 'alerts')" }),
                payload: Type.String({ description: "Message content" }),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const { topic, payload } = params;
                const agentId = _context?.agentId || "main";
                const store = await ensureStore(agentId);
                const client = store.getMcpClient();
                if (!client)
                    return { content: [{ type: "text", text: "MCP client not available" }] };
                const result = await client.callTool("ghost_publish", { topic, payload, publisher: agentId });
                return { content: [{ type: "text", text: JSON.stringify(result) }] };
            },
        });
        api.registerTool({
            name: "tachi_kanban_post",
            label: "Kanban Post",
            description: "Post a kanban card to another agent or broadcast.",
            parameters: Type.Object({
                to_agent: Type.String({ description: "Destination agent ID, or '*' for broadcast" }),
                title: Type.String({ description: "Card title" }),
                body: Type.String({ description: "Card body content" }),
                priority: Type.Optional(Type.String({ description: "Priority: low | medium | high | critical (default: medium)" })),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const { to_agent, title, body, priority } = params;
                const agentId = _context?.agentId || "main";
                const store = await ensureStore(agentId);
                const client = store.getMcpClient();
                if (!client)
                    return { content: [{ type: "text", text: "MCP client not available" }] };
                const result = await client.callTool("post_card", { from_agent: agentId, to_agent, title, body, priority: priority || "medium" });
                return { content: [{ type: "text", text: JSON.stringify(result) }] };
            },
        });
        api.registerTool({
            name: "tachi_save_memory",
            label: "Save Memory (Tachi)",
            description: "Save a memory entry directly to Tachi with full metadata.",
            parameters: Type.Object({
                text: Type.String({ description: "Memory text content" }),
                path: Type.Optional(Type.String({ description: "Hierarchical path (e.g. /project/notes)" })),
                importance: Type.Optional(Type.Number({ description: "0.0-1.0 importance score" })),
                keywords: Type.Optional(Type.Array(Type.String(), { description: "Keyword tags" })),
                category: Type.Optional(Type.String({ description: "Category: fact | decision | preference | entity | other" })),
            }),
            async execute(_toolCallId, params, _signal, _context) {
                const { text, path: memPath, importance, keywords, category } = params;
                const agentId = _context?.agentId || "main";
                const store = await ensureStore(agentId);
                const client = store.getMcpClient();
                if (!client)
                    return { content: [{ type: "text", text: "MCP client not available" }] };
                const result = await client.callTool("save_memory", {
                    text,
                    path: memPath || `/openclaw/agent-${agentId}`,
                    importance: importance ?? 0.7,
                    keywords: keywords || [],
                    category: category || "fact",
                });
                return { content: [{ type: "text", text: JSON.stringify(result) }] };
            },
        });
        // C1 fix: accept ctx as 2nd param — agentId lives in ctx, not event
        api.on("before_agent_start", async (event, ctx) => {
            const query = event.prompt;
            if (!query || query.length < 5)
                return;
            const agentId = ctx?.agentId || "main";
            try {
                const store = await ensureStore(agentId);
                const client = store.getMcpClient();
                if (client) {
                    try {
                        const recall = await client.recallContext(query, {
                            top_k: config.topK,
                            candidate_multiplier: 1,
                            agent_id: agentId,
                            exclude_topics: ["imsg_conversation"],
                        });
                        if (recall.prependContext.trim()) {
                            return { prependContext: recall.prependContext };
                        }
                        return;
                    }
                    catch (err) {
                        api.logger.warn(`tachi [${agentId}]: server-side recall failed, fallback to local FTS: ${String(err)}`);
                    }
                }
                const hits = await performFtsSearch(query, agentId);
                // Filter out iMessage conversation chunks — private chats should not leak into agent context
                const filtered = hits.filter(h => h.entry.topic !== "imsg_conversation");
                if (filtered.length === 0)
                    return;
                const memoryLines = filtered.map((h, i) => {
                    const e = h.entry;
                    // L0 injection: summary + metadata only; use memory_get for full text (L2)
                    return [
                        `M-ENTRY #${i + 1} [ID=${e.id}] [Topic=${e.topic}] [Score=${h.final_score.toFixed(2)}]`,
                        `Summary: ${e.summary || e.text.substring(0, 80)}`,
                        `Keywords: ${e.keywords.join(", ")} | Persons: ${e.persons.join(", ")}`,
                    ].join("\n");
                });
                // Extract entity graph connections from results
                const allEntities = new Set();
                for (const h of filtered) {
                    if (h.entry.entities) {
                        for (const e of h.entry.entities)
                            allEntities.add(e);
                    }
                }
                const entityLinks = [];
                if (allEntities.size > 0) {
                    // Group entities by which memories mention them together
                    for (const h of filtered) {
                        if (h.entry.entities && h.entry.entities.length >= 2) {
                            entityLinks.push(h.entry.entities.join(" ↔ "));
                        }
                    }
                }
                let injectBlock = `\n<relevant-structured-memories>\n${memoryLines.join("\n\n")}\n`;
                if (entityLinks.length > 0) {
                    const uniqueLinks = [...new Set(entityLinks)];
                    injectBlock += `\nEntity connections: ${uniqueLinks.join(", ")}\n`;
                }
                injectBlock += `</relevant-structured-memories>\n`;
                return { prependContext: injectBlock };
            }
            catch (err) {
                api.logger.warn(`tachi [${agentId}]: recall failed: ${String(err)}`);
            }
        });
        // C1 fix: accept ctx as 2nd param — agentId lives in ctx, not event
        api.on("agent_end", async (event, ctx) => {
            if (!event.success || !event.messages || event.messages.length === 0)
                return;
            const agentId = ctx?.agentId || "main";
            // W4 fix: extract text from structured content blocks
            function msgToText(m) {
                const c = m?.content;
                if (typeof c === "string")
                    return c;
                if (Array.isArray(c))
                    return c.filter((b) => b?.type === "text").map((b) => b.text || "").join("\n");
                return String(c || "");
            }
            // C2 fix: check ALL recent messages for trigger, not just the last one
            const recentMsgs = event.messages.slice(-6);
            const fullText = recentMsgs.map(msgToText).join("\n");
            const lower = fullText.toLowerCase();
            // W2 fix: guard captureTriggerKeywords with Array.isArray
            const keywords = Array.isArray(config.captureTriggerKeywords) ? config.captureTriggerKeywords : [];
            const triggered = keywords.some((kw) => lower.includes(kw.toLowerCase()));
            if (!triggered && fullText.length < config.captureMinChars)
                return;
            // Circuit breaker: skip extraction if LLM is known to be down
            if (Date.now() < extractBackoffUntil)
                return;
            try {
                const { audit } = getResolvedPaths(agentId);
                const spoolPath = getCaptureSpoolPath(audit);
                const sessionKey = ctx?.sessionKey || `s_${Date.now()}`;
                const conversationId = ctx?.conversationId || ctx?.sessionId || `openclaw:${agentId}`;
                const capturePayload = {
                    conversation_id: conversationId,
                    turn_id: sessionKey,
                    agent_id: agentId,
                    messages: recentMsgs.map((m) => ({
                        role: typeof m?.role === "string" ? m.role : "unknown",
                        content: msgToText(m),
                    })),
                    path_prefix: `/openclaw/agent-${agentId}`,
                };
                const runtimeStore = await ensureStore(agentId);
                const client = runtimeStore.getMcpClient();
                if (!client) {
                    const queued = await enqueueCapturePayload(spoolPath, capturePayload);
                    noteCaptureFailure(agentId, "MCP client unavailable");
                    api.logger.warn(`tachi [${agentId}]: queued capture payload locally (${queued} pending)`);
                    return;
                }
                try {
                    const replay = await flushCaptureSpool(spoolPath, client, api.logger, agentId);
                    if (replay.replayed > 0) {
                        api.logger.info(`tachi [${agentId}]: replayed ${replay.replayed} queued capture payloads`);
                    }
                    const capture = await client.captureSession(capturePayload);
                    const status = typeof capture?.status === "string" ? capture.status : "unknown";
                    const captured = typeof capture?.captured === "number" ? capture.captured : 0;
                    markCaptureHealthy();
                    if (captured > 0 || status === "completed") {
                        try {
                            await appendAuditLog(audit, {
                                action: "capture_session",
                                status,
                                captured,
                                entry_ids: Array.isArray(capture?.ids) ? capture.ids : [],
                                merged_ids: Array.isArray(capture?.merged_ids) ? capture.merged_ids : [],
                                duplicates_skipped: typeof capture?.duplicates_skipped === "number"
                                    ? capture.duplicates_skipped
                                    : 0,
                                agent: agentId,
                            });
                        }
                        catch (auditErr) {
                            api.logger.warn(`tachi [${agentId}]: audit-log write failed: ${String(auditErr)}`);
                        }
                    }
                    if (status === "completed" || status === "skipped") {
                        if (captured > 0) {
                            api.logger.info(`tachi [${agentId}]: captured ${captured} memories via Tachi`);
                        }
                        return;
                    }
                    const queued = await enqueueCapturePayload(spoolPath, capturePayload);
                    noteCaptureFailure(agentId, `unexpected capture status: ${status}`);
                    api.logger.warn(`tachi [${agentId}]: queued capture payload locally (${queued} pending)`);
                    return;
                }
                catch (err) {
                    const queued = await enqueueCapturePayload(spoolPath, capturePayload);
                    noteCaptureFailure(agentId, err);
                    api.logger.warn(`tachi [${agentId}]: queued capture payload locally (${queued} pending)`);
                    return;
                }
            }
            catch (err) {
                noteCaptureFailure(agentId, err);
            }
        });
        api.registerService({
            id: "tachi",
            start: () => api.logger.info("tachi: service started"),
            stop: async () => {
                api.logger.info("tachi: shutting down...");
                const entries = [...initStores.entries()];
                initStores.clear();
                for (const [, storePromise] of entries) {
                    try {
                        const store = await storePromise;
                        await store.close();
                    }
                    catch { /* store never initialized successfully */ }
                }
                api.logger.info("tachi: service stopped");
            },
        });
    },
};
export default memoryHybridBridgePlugin;
