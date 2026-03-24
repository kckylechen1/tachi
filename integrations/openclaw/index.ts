import fs from "node:fs/promises";
import path from "node:path";
import { Type } from "@sinclair/typebox";
import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import { bridgeConfigSchema, pluginDataDir, workspaceRoot, type MemoryEntry } from "./config.js";
import { extractMemoryEntry, getEmbedding, mergeMemoryEntries } from "./extractor.js";
import { getStore, MemoryStore } from "./store.js";
import { rerank } from "./reranker.js";

// ============================================================================
// Internal Helpers
// ============================================================================

async function ensureFile(filePath: string): Promise<void> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  try {
    await fs.access(filePath);
  } catch {
    await fs.writeFile(filePath, "", "utf8");
  }
}

async function appendAuditLog(
  auditLogPath: string,
  payload: Record<string, unknown>,
): Promise<void> {
  await ensureFile(auditLogPath);
  const line = { ts: new Date().toISOString(), ...payload };
  await fs.appendFile(auditLogPath, `${JSON.stringify(line)}\n`, "utf8");
}

function resolveConfigPath(api: OpenClawPluginApi, configuredPath: string): string {
  return path.isAbsolute(configuredPath) ? configuredPath : api.resolvePath(configuredPath);
}

// ============================================================================
// Plugin Definition
// ============================================================================

export const memoryHybridBridgePlugin = {
  id: "memory-hybrid-bridge",
  name: "Memory Hybrid Bridge",
  kind: "memory" as const,
  description:
    "Advanced structured memory with LLM extraction and hybrid retrieval (vector/lexical/symbolic)",

  register(api: OpenClawPluginApi) {
    const config = bridgeConfigSchema.parse(api.pluginConfig);

    const initStores = new Map<string, Promise<MemoryStore>>();

    function getResolvedPaths(agentId?: string) {
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

    async function ensureStore(agentId?: string): Promise<MemoryStore> {
      const paths = getResolvedPaths(agentId);
      if (!initStores.has(paths.db)) {
        // C3 fix: don't cache rejected promises — delete on failure so next call retries
        const promise = getStore(paths.db, paths.shadow, api.logger).catch((err) => {
          initStores.delete(paths.db);
          throw err;
        });
        initStores.set(paths.db, promise);
      }
      return await initStores.get(paths.db)!;
    }

    api.logger.info(`memory-hybrid-bridge: registered (dynamic agent-scoping enabled)`);

    // --- Search Logic ---
    // Full hybrid search (FTS + Vec) — calls Voyage API for query embedding.
    // Used by user-initiated tool calls (memory_search, memory_hybrid_search).
    async function performSearch(query: string, agentId?: string, searchTopK?: number) {
      const store = await ensureStore(agentId);
      const topK = searchTopK ?? config.topK;
      const queryVector = await getEmbedding({ config, text: query, logger: api.logger });

      // Pull more candidates for reranking (3× topK), then rerank down to topK
      const candidates = topK * 3;
      try {
        const { docs, scores } = await store.search(
          query,
          queryVector ?? undefined,
          {
            top_k: candidates,
            weights: config.weights
          }
        );

        const hybridResults = docs.map((doc) => ({ final_score: scores[doc.id] ?? 0, entry: doc }));

        // Rerank via Voyage rerank-2.5 (falls back to hybrid order on failure)
        return rerank({ config, query, results: hybridResults, topK, logger: api.logger });
      } catch (err) {
        // Fallback path: keep memory search available if vector channel fails.
        api.logger.warn(`memory-hybrid-bridge: hybrid search failed, fallback to FTS: ${String(err)}`);
        const { docs, scores } = await store.search(
          query,
          undefined,
          {
            top_k: candidates,
            weights: config.weights
          }
        );
        const hybridResults = docs.map((doc) => ({ final_score: scores[doc.id] ?? 0, entry: doc }));
        return rerank({ config, query, results: hybridResults, topK, logger: api.logger });
      }
    }

    // FTS-only search — zero network calls, used for automatic context injection.
    // Restores the old architecture's zero-latency search for before_agent_start.
    async function performFtsSearch(query: string, agentId?: string, searchTopK?: number) {
      const store = await ensureStore(agentId);
      const topK = searchTopK ?? config.topK;

      const { docs, scores } = await store.search(
        query,
        undefined,
        {
          top_k: topK,
          weights: config.weights
        }
      );

      return docs.map((doc) => ({ final_score: scores[doc.id] ?? 0, entry: doc }));
    }

    // ========================================================================
    // Tools — register as memory_search / memory_get so the agent's
    // natural tool calls hit the hybrid shadow store directly.
    // Requires plugins.slots.memory = "memory-hybrid-bridge" in openclaw.json.
    // ========================================================================

    function formatSearchResults(
      hits: Array<{ final_score: number; entry: MemoryEntry }>,
    ) {
      if (hits.length === 0) {
        return {
          content: [{ type: "text" as const, text: "No relevant memories found." }],
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
        content: [{ type: "text" as const, text: JSON.stringify({ results }) }],
        details: { count: hits.length, results },
      };
    }

    api.registerTool({
      name: "memory_search",
      label: "Memory Search",
      description:
        "Mandatory recall step: semantically search long-term structured memory before answering questions about prior work, decisions, dates, people, preferences, or todos; returns top snippets with relevance scores.",
      parameters: Type.Object({
        query: Type.String({ description: "Natural language search query" }),
        maxResults: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
        minScore: Type.Optional(Type.Number({ description: "Min score threshold (default: 0)" })),
      }),
      async execute(_toolCallId, params, _signal, _context) {
        const { query, maxResults, minScore } = params as {
          query: string;
          maxResults?: number;
          minScore?: number;
        };
        const agentId = (_context as any)?.agentId || "main";
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
      description:
        "Search long-term structured memory using vector, lexical, and symbolic hybrid scoring.",
      parameters: Type.Object({
        query: Type.String({ description: "Natural language search query" }),
        top_k: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
      }),
      async execute(_toolCallId, params, _signal, _context) {
        const { query, top_k } = params as { query: string; top_k?: number };
        const agentId = (_context as any)?.agentId || "main";
        const hits = await performSearch(query, agentId, top_k);
        return formatSearchResults(hits);
      },
    });

    api.registerTool({
      name: "memory_get",
      label: "Memory Get",
      description:
        "Retrieve a specific memory entry by id from the shadow store; use after memory_search to get full details.",
      parameters: Type.Object({
        path: Type.String({
          description: "Entry id (e.g. shadow-store/m_1234) or raw id (m_1234)",
        }),
        from: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
        lines: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
      }),
      async execute(_toolCallId, params, _signal, _context) {
        const rawPath = (params as { path: string }).path;
        const entryId = rawPath.replace(/^shadow-store\//, "");
        const agentId = (_context as any)?.agentId || "main";
        const store = await ensureStore(agentId);
        const found = await store.get(entryId);

        if (!found) {
          return {
            content: [
              {
                type: "text" as const,
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
          content: [{ type: "text" as const, text: JSON.stringify({ path: rawPath, text }) }],
          details: { found: true },
        };
      },
    });

    // C1 fix: accept ctx as 2nd param — agentId lives in ctx, not event
    api.on("before_agent_start", async (event: any, ctx: any) => {
      const query = event.prompt;
      if (!query || query.length < 5) return;
      const agentId = ctx?.agentId || "main";

      try {
        const hits = await performFtsSearch(query, agentId);
        // Filter out iMessage conversation chunks — private chats should not leak into agent context
        const filtered = hits.filter(h => h.entry.topic !== "imsg_conversation");
        if (filtered.length === 0) return;

        const memoryLines = filtered.map((h, i) => {
          const e = h.entry;
          // L0 injection: summary + metadata only; use memory_get for full text (L2)
          return [
            `M-ENTRY #${i + 1} [ID=${e.id}] [Topic=${e.topic}] [Score=${h.final_score.toFixed(2)}]`,
            `Summary: ${e.summary || e.text.substring(0, 80)}`,
            `Keywords: ${e.keywords.join(", ")} | Persons: ${e.persons.join(", ")}`,
          ].join("\n");
        });

        const injectBlock = `\n<relevant-structured-memories>\n${memoryLines.join("\n\n")}\n</relevant-structured-memories>\n`;

        return { prependContext: injectBlock };
      } catch (err) {
        api.logger.warn(`memory-hybrid-bridge [${agentId}]: recall failed: ${String(err)}`);
      }
    });

    // C1 fix: accept ctx as 2nd param — agentId lives in ctx, not event
    api.on("agent_end", async (event: any, ctx: any) => {
      if (!event.success || !event.messages || event.messages.length === 0) return;
      const agentId = ctx?.agentId || "main";

      // W4 fix: extract text from structured content blocks
      function msgToText(m: any): string {
        const c = m?.content;
        if (typeof c === "string") return c;
        if (Array.isArray(c)) return c.filter((b: any) => b?.type === "text").map((b: any) => b.text || "").join("\n");
        return String(c || "");
      }

      // C2 fix: check ALL recent messages for trigger, not just the last one
      const recentMsgs = (event.messages as any[]).slice(-6);
      const fullText = recentMsgs.map(msgToText).join("\n");
      const lower = fullText.toLowerCase();

      // W2 fix: guard captureTriggerKeywords with Array.isArray
      const keywords = Array.isArray(config.captureTriggerKeywords) ? config.captureTriggerKeywords : [];
      const triggered = keywords.some((kw: string) => lower.includes(kw.toLowerCase()));

      if (!triggered && fullText.length < config.captureMinChars) return;

      try {
        const { audit } = getResolvedPaths(agentId);

        const windowText = recentMsgs
          .map((m: any, i: number) => `[${i}] ${m.role}: ${msgToText(m)}`)
          .join("\n\n");

        const extracted = await extractMemoryEntry({
          config,
          inputWindowText: windowText,
          sourceRefId: ctx?.sessionKey || `s_${Date.now()}`,
          agentId,
          logger: api.logger,
        });

        if (!extracted) return;

        const vector = await getEmbedding({
          config,
          text: extracted.text,
          logger: api.logger,
        });
        if (vector) extracted.vector = vector;

        const store = await ensureStore(agentId);

        // Dedup + Merge via vector similarity (Semantic Synthesis)
        // >= dedupThreshold (0.95): exact duplicate, skip
        // >= mergeThreshold (0.85): related, merge via LLM
        // < mergeThreshold: new entry, insert directly
        if (extracted.vector && extracted.vector.length > 0) {
          let similar: Array<{ entry: MemoryEntry; similarity: number }> = [];
          try {
            similar = await store.findSimilar(extracted.vector, 1);
          } catch (similarErr) {
            // sqlite-vec KNN can fail across vec0 versions.
            // Dedup is best-effort; don't block memory writes.
            api.logger.warn(
              `memory-hybrid-bridge [${agentId}]: findSimilar failed; skip dedup: ${String(similarErr)}`,
            );
          }
          if (similar.length > 0) {
            const top = similar[0];
            if (top.similarity >= config.dedupThreshold) {
              api.logger.info(`memory-hybrid-bridge [${agentId}]: skipping duplicate (sim=${top.similarity.toFixed(3)})`);
              return;
            }
            if (top.similarity >= config.mergeThreshold) {
              api.logger.info(`memory-hybrid-bridge [${agentId}]: merging with ${top.entry.id} (sim=${top.similarity.toFixed(3)})`);
              const merged = await mergeMemoryEntries({ config, existing: top.entry, incoming: extracted, logger: api.logger });
              if (merged) {
                const mergedVec = await getEmbedding({ config, text: merged.text, logger: api.logger });
                if (mergedVec) merged.vector = mergedVec;
                await store.upsert(merged);
                // W5 fix: audit-log in separate try/catch
                try {
                  await appendAuditLog(audit, {
                    action: "merge",
                    entry_id: merged.id,
                    merged_with: extracted.id,
                    similarity: top.similarity,
                    agent: agentId,
                  });
                } catch (auditErr) {
                  api.logger.warn(`memory-hybrid-bridge [${agentId}]: audit-log write failed: ${String(auditErr)}`);
                }
                api.logger.info(`memory-hybrid-bridge [${agentId}]: merged memory: ${merged.id}`);
                return;
              }
              // Merge failed, fall through to insert as new
            }
          }
        }

        await store.upsert(extracted);
        // W5 fix: audit-log in separate try/catch
        try {
          await appendAuditLog(audit, {
            action: "append",
            entry_id: extracted.id,
            agent: agentId,
          });
        } catch (auditErr) {
          api.logger.warn(`memory-hybrid-bridge [${agentId}]: audit-log write failed: ${String(auditErr)}`);
        }
        api.logger.info(
          `memory-hybrid-bridge [${agentId}]: auto-captured new memory: ${extracted.id}`,
        );
      } catch (err) {
        api.logger.warn(
          `memory-hybrid-bridge [${agentId}]: auto-capture failed: ${String(err)}`,
        );
      }
    });

    api.registerService({
      id: "memory-hybrid-bridge",
      start: () => api.logger.info("memory-hybrid-bridge: service started"),
      stop: () => api.logger.info("memory-hybrid-bridge: service stopped"),
    });
  },
};

export default memoryHybridBridgePlugin;
