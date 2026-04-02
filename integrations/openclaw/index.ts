import path from "node:path";
import { Type } from "@sinclair/typebox";
import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import { bridgeConfigSchema, type MemoryEntry } from "./config.js";
import { MemoryMcpClient } from "./mcp-client.js";

type SearchHit = {
  final_score: number;
  entry: MemoryEntry;
};

type SearchResult =
  | { available: true; hits: SearchHit[] }
  | { available: false; message: string };

function resolveConfigPath(api: OpenClawPluginApi, configuredPath: string): string {
  return path.isAbsolute(configuredPath) ? configuredPath : api.resolvePath(configuredPath);
}

function textResult(text: string, details?: Record<string, unknown>) {
  return {
    content: [{ type: "text" as const, text }],
    ...(details ? { details } : {}),
  };
}

// ============================================================================
// Plugin Definition
// ============================================================================

export const memoryHybridBridgePlugin = {
  id: "tachi",
  name: "Memory Hybrid Bridge",
  kind: "memory" as const,
  description:
    "Advanced structured memory with LLM extraction and hybrid retrieval (vector/lexical/symbolic)",

  register(api: OpenClawPluginApi) {
    const config = bridgeConfigSchema.parse(api.pluginConfig);
    const dbPath = resolveConfigPath(api, config.dbPath);
    let initClient: Promise<MemoryMcpClient> | null = null;

    function ensureClient(): Promise<MemoryMcpClient> {
      if (!initClient) {
        initClient = Promise.resolve(new MemoryMcpClient(dbPath, api.logger));
      }
      return initClient;
    }

    async function runWithClient<T>(
      operation: string,
      run: (client: MemoryMcpClient) => Promise<T>,
    ): Promise<{ ok: true; value: T } | { ok: false; error: unknown }> {
      try {
        const client = await ensureClient();
        return { ok: true, value: await run(client) };
      } catch (error) {
        api.logger.warn(`tachi: ${operation} unavailable: ${String(error)}`);
        return { ok: false, error };
      }
    }

    async function performSearch(query: string, searchTopK?: number): Promise<SearchResult> {
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

    async function performRecall(query: string, agentId?: string) {
      const result = await runWithClient("recall_context", async (client) =>
        await client.recallContext(query, {
          top_k: config.topK,
          candidate_multiplier: 1,
          agent_id: agentId,
          exclude_topics: ["imsg_conversation"],
        }),
      );

      return result.ok ? result.value : null;
    }

    function formatSearchResults(result: SearchResult) {
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
      description:
        "Mandatory recall step: semantically search long-term structured memory before answering questions about prior work, decisions, dates, people, preferences, or todos; returns top snippets with relevance scores.",
      parameters: Type.Object({
        query: Type.String({ description: "Natural language search query" }),
        maxResults: Type.Optional(Type.Number({ description: "Max results (default: 6)" })),
        minScore: Type.Optional(Type.Number({ description: "Min score threshold (default: 0)" })),
      }),
      async execute(_toolCallId, params) {
        const { query, maxResults, minScore } = params as {
          query: string;
          maxResults?: number;
          minScore?: number;
        };

        const result = await performSearch(query, maxResults ?? config.topK);
        if (!result.available) {
          return formatSearchResults(result);
        }

        const hits =
          typeof minScore === "number" && minScore > 0
            ? result.hits.filter((hit) => hit.final_score >= minScore)
            : result.hits;

        return formatSearchResults({ available: true, hits });
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
      async execute(_toolCallId, params) {
        const { query, top_k } = params as { query: string; top_k?: number };
        return formatSearchResults(await performSearch(query, top_k));
      },
    });

    api.registerTool({
      name: "memory_get",
      label: "Memory Get",
      description:
        "Retrieve a specific memory entry by id; use after memory_search to get full details.",
      parameters: Type.Object({
        path: Type.String({
          description: "Entry id (e.g. memory/m_1234) or raw id (m_1234)",
        }),
        from: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
        lines: Type.Optional(Type.Number({ description: "Ignored (compat)" })),
      }),
      async execute(_toolCallId, params) {
        const rawPath = (params as { path: string }).path;
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
          return textResult(
            JSON.stringify({
              path: rawPath,
              text: "",
              error: `Memory entry not found: ${entryId}`,
            }),
            { available: true, found: false },
          );
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

    // ── Tachi Passthrough Tools ──────────────────────────────────

    api.registerTool({
      name: "tachi_ghost_publish",
      label: "Ghost Whisper",
      description: "Publish a message to a Ghost Whispers topic for inter-agent communication.",
      parameters: Type.Object({
        topic: Type.String({ description: "Topic name (e.g. 'build-status', 'alerts')" }),
        payload: Type.String({ description: "Message content" }),
      }),
      async execute(_toolCallId, params, _signal, context) {
        const { topic, payload } = params as { topic: string; payload: string };
        const agentId = (context as any)?.agentId || "main";
        const result = await runWithClient("ghost_publish", async (client) =>
          await client.callTool("ghost_publish", { topic, payload, publisher: agentId }),
        );

        return result.ok
          ? textResult(JSON.stringify(result.value))
          : textResult("Tachi MCP client unavailable.");
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
      async execute(_toolCallId, params, _signal, context) {
        const { to_agent, title, body, priority } = params as {
          to_agent: string;
          title: string;
          body: string;
          priority?: string;
        };
        const agentId = (context as any)?.agentId || "main";
        const result = await runWithClient("post_card", async (client) =>
          await client.callTool("post_card", {
            from_agent: agentId,
            to_agent,
            title,
            body,
            priority: priority || "medium",
          }),
        );

        return result.ok
          ? textResult(JSON.stringify(result.value))
          : textResult("Tachi MCP client unavailable.");
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
      async execute(_toolCallId, params, _signal, context) {
        const { text, path: memoryPath, importance, keywords, category } = params as {
          text: string;
          path?: string;
          importance?: number;
          keywords?: string[];
          category?: string;
        };
        const agentId = (context as any)?.agentId || "main";
        const result = await runWithClient("save_memory", async (client) =>
          await client.callTool("save_memory", {
            text,
            path: memoryPath || `/openclaw/agent-${agentId}`,
            importance: importance ?? 0.7,
            keywords: keywords || [],
            category: category || "fact",
          }),
        );

        return result.ok
          ? textResult(JSON.stringify(result.value))
          : textResult("Tachi MCP client unavailable.");
      },
    });

    api.registerTool({
      name: "tachi_capture_session",
      label: "Capture Session (Tachi)",
      description: "Forward a conversation turn to Tachi for background extraction.",
      parameters: Type.Object({
        conversation_id: Type.String({ description: "Stable conversation identifier" }),
        turn_id: Type.String({ description: "Unique turn identifier" }),
        messages: Type.Array(
          Type.Object({
            role: Type.String({ description: "Message role" }),
            content: Type.String({ description: "Message text content" }),
          }),
          { description: "Conversation messages for this turn" },
        ),
        path_prefix: Type.Optional(Type.String({ description: "Optional memory path prefix" })),
        scope: Type.Optional(Type.String({ description: "Optional memory scope" })),
        force: Type.Optional(Type.Boolean({ description: "Force processing even if dedupe would skip" })),
      }),
      async execute(_toolCallId, params, _signal, context) {
        const { conversation_id, turn_id, messages, path_prefix, scope, force } = params as {
          conversation_id: string;
          turn_id: string;
          messages: Array<{ role: string; content: string }>;
          path_prefix?: string;
          scope?: string;
          force?: boolean;
        };
        const agentId = (context as any)?.agentId || "main";
        const result = await runWithClient("capture_session", async (client) =>
          await client.captureSession({
            conversation_id,
            turn_id,
            agent_id: agentId,
            messages,
            path_prefix,
            scope,
            force,
          }),
        );

        return result.ok
          ? textResult(JSON.stringify(result.value))
          : textResult("Tachi MCP client unavailable.");
      },
    });

    api.on("before_agent_start", async (event: any, context: any) => {
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
          } catch {
            // Client never initialized successfully.
          }
        }
        api.logger.info("tachi: service stopped");
      },
    });
  },
};

export default memoryHybridBridgePlugin;
