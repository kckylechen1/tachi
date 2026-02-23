import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import { Type } from "@sinclair/typebox";
// @ts-ignore
import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import { bridgeConfigSchema, type BridgeConfig, type MemoryEntry } from "./config.js";
import { extractMemoryEntry, getEmbedding, validateMemoryEntry } from "./extractor.js";
import { cosineSimilarity } from "./scorer.js";
import { getStore, MemoryStore } from "./store.js";

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

// Removed raw JSONL reading function readShadowEntries. It is handled by SQLite now.

function isDuplicateEntry(
  candidate: MemoryEntry,
  existing: MemoryEntry[],
  threshold: number,
): boolean {
  if (candidate.vector && candidate.vector.length > 0) {
    for (const e of existing) {
      if (e.vector && e.vector.length === candidate.vector.length) {
        if (cosineSimilarity(candidate.vector, e.vector) >= threshold) return true;
      }
    }
  }
  const cText = candidate.text.toLowerCase();
  for (const e of existing) {
    const eText = e.text.toLowerCase();
    if (eText.includes(cText) || cText.includes(eText)) return true;
  }
  return false;
}

// ============================================================================
// Plugin Definition
// ============================================================================

export const memoryHybridBridgePlugin = {
  id: "memory-hybrid-bridge",
  name: "Memory Hybrid Bridge",
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
          db: api.resolvePath(config.dbPath),
          shadow: api.resolvePath(config.shadowStorePath),
          audit: api.resolvePath(config.auditLogPath),
        };
      }

      // Per-agent scoped memory directory
      const agentMemDir = path.resolve(
        process.env.OPENCLAW_WORKSPACE || process.cwd(),
        `agents/${id}/memory`,
      );
      return {
        db: path.resolve(agentMemDir, "memory.db"),
        shadow: path.resolve(agentMemDir, "shadow-store.jsonl"),
        audit: path.resolve(agentMemDir, "audit-log.jsonl"),
      };
    }

    async function ensureStore(agentId?: string): Promise<MemoryStore> {
      const paths = getResolvedPaths(agentId);
      if (!initStores.has(paths.db)) {
        initStores.set(paths.db, getStore(paths.db, paths.shadow, api.logger));
      }
      return await initStores.get(paths.db)!;
    }

    api.logger.info(`memory-hybrid-bridge: registered (dynamic agent-scoping enabled)`);

    // --- Search Logic ---
    async function performSearch(query: string, agentId?: string, searchTopK?: number) {
      const store = await ensureStore(agentId);
      const topK = searchTopK ?? config.topK;
      const queryVector = await getEmbedding({ config, text: query, logger: api.logger });

      const { docs, scores } = store.search(
        query,
        queryVector,
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
    // Requires plugins.slots.memory = "none" in openclaw.json to avoid
    // conflict with the built-in memory-core plugin.
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
        const found = store.get(entryId);

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

    api.on("before_agent_start", async (event: any) => {
      const query = event.prompt;
      if (!query || query.length < 5) return;

      try {
        const hits = await performSearch(query, (event as any).agentId);
        if (hits.length === 0) return;

        const memoryLines = hits.map((h, i) => {
          const e = h.entry;
          return [
            `M-ENTRY #${i + 1} [ID=${e.id}] [Topic=${e.topic}]`,
            `Fact: ${e.text}`,
            `Context: ${e.keywords.join(", ")} | ${e.persons.join(", ")}`,
          ].join("\n");
        });

        const injectBlock = `\n<relevant-structured-memories>\n${memoryLines.join("\n\n")}\n</relevant-structured-memories>\n`;

        return { prependContext: injectBlock };
      } catch (err) {
        api.logger.warn(`memory-hybrid-bridge: recall failed: ${String(err)}`);
      }
    });

    api.on("agent_end", async (event: any) => {
      if (!event.success || !event.messages || event.messages.length === 0) return;

      const lastMsg = event.messages.at(-1) as any;
      const text = String(lastMsg?.content || "");
      const lower = text.toLowerCase();
      const triggered = config.captureTriggerKeywords.some((kw: string) =>
        lower.includes(kw.toLowerCase()),
      );

      if (!triggered && text.length < config.captureMinChars) return;

      try {
        const agentId = (event as any).agentId;
        const { shadow, audit } = getResolvedPaths(agentId);

        const windowText = event.messages
          .slice(-6)
          .map((m: any, i: number) => `[${i}] ${m.role}: ${String(m.content || "")}`)
          .join("\n\n");

        const extracted = await extractMemoryEntry({
          config,
          inputWindowText: windowText,
          sourceRefId: (event as any).id || `s_${Date.now()}`,
          agentId: agentId || "main",
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

        // Basic dedup logic
        const existing = store.getAll(200); // Only check against latest 200 for dedup to be fast
        if (isDuplicateEntry(extracted, existing, config.dedupThreshold)) {
          api.logger.info(`memory-hybrid-bridge [${agentId}]: skipping duplicate memory entry`);
          return;
        }

        store.upsert(extracted);
        await appendAuditLog(audit, {
          action: "append",
          entry_id: extracted.id,
          agent: agentId,
        });
        api.logger.info(
          `memory-hybrid-bridge [${agentId}]: auto-captured new memory: ${extracted.id}`,
        );
      } catch (err) {
        api.logger.warn(
          `memory-hybrid-bridge [(event as any).agentId]: auto-capture failed: ${String(err)}`,
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
