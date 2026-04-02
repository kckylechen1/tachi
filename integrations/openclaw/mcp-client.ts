import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import type { MemoryEntry } from "./config.js";

export type HybridScore = {
  vector: number;
  fts: number;
  symbolic: number;
  decay: number;
  final: number;
};

type SearchPayload = {
  docs: MemoryEntry[];
  scores: Record<string, number>;
  scoreBreakdowns: Record<string, HybridScore>;
};

type LoggerLike = {
  info?: (message: string) => void;
  warn?: (message: string) => void;
};

type SearchOptions = {
  top_k?: number;
  candidates?: number;
  path_prefix?: string;
  record_access?: boolean;
  weights?: { semantic: number; fts: number; symbolic: number; decay: number };
};

type RecallContextOptions = {
  top_k?: number;
  candidate_multiplier?: number;
  path_prefix?: string;
  agent_id?: string;
  exclude_topics?: string[];
  min_score?: number;
};

type LaunchConfig = {
  command: string;
  args: string[];
  cwd: string;
  env: Record<string, string>;
};

type RawToolResult = CallToolResult & {
  structuredContent?: unknown;
  content?: Array<Record<string, unknown>>;
  isError?: boolean;
};

const REQUIRED_TOOLS = [
  "recall_context",
  "capture_session",
  "save_memory",
  "search_memory",
  "get_memory",
  "delete_memory",
  "memory_stats",
  "list_memories",
] as const;

function asFiniteNumber(value: unknown): number {
  const n = typeof value === "number" ? value : Number(value);
  return Number.isFinite(n) ? n : 0;
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function asStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.filter((v): v is string => typeof v === "string");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isSourceRef(value: unknown): value is MemoryEntry["metadata"]["source_refs"][number] {
  return (
    isRecord(value) &&
    typeof value.ref_type === "string" &&
    typeof value.ref_id === "string"
  );
}

function ensureMetadata(value: unknown): MemoryEntry["metadata"] {
  if (!isRecord(value)) {
    return { source_refs: [] };
  }
  const sourceRefsRaw = value.source_refs;
  const sourceRefs = Array.isArray(sourceRefsRaw)
    ? sourceRefsRaw.filter((item): item is MemoryEntry["metadata"]["source_refs"][number] =>
        isSourceRef(item),
      )
    : [];
  return {
    source_refs: sourceRefs,
    ...value,
  };
}

function extractTextBlocks(content: unknown): string[] {
  if (!Array.isArray(content)) {
    return [];
  }
  const out: string[] = [];
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

function coerceMemoryEntry(raw: unknown): MemoryEntry | undefined {
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
    category: (asString(raw.category) || "other") as MemoryEntry["category"],
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

function extractErrorMessage(result: RawToolResult, toolName: string): string {
  const text = extractTextBlocks(result.content).find(Boolean);
  return text || `MCP tool "${toolName}" returned an error`;
}

function extractJsonPayload<T>(result: RawToolResult, toolName: string): T {
  if (result.structuredContent !== undefined) {
    if (typeof result.structuredContent === "string") {
      return JSON.parse(result.structuredContent) as T;
    }
    return result.structuredContent as T;
  }

  const textBlocks = extractTextBlocks(result.content).filter((text) => text.trim().length > 0);

  for (const text of textBlocks) {
    try {
      return JSON.parse(text) as T;
    } catch {
      continue;
    }
  }

  throw new Error(`MCP tool "${toolName}" returned non-JSON content`);
}

export class MemoryMcpClient {
  private client: Client | null = null;
  private transport: StdioClientTransport | null = null;
  private connecting: Promise<Client> | null = null;
  private availableTools = new Set<string>();

  constructor(
    private readonly dbPath: string,
    private readonly logger?: LoggerLike,
  ) {}

  private logInfo(message: string): void {
    this.logger?.info?.(`memory-hybrid-bridge[mcp]: ${message}`);
  }

  private logWarn(message: string): void {
    this.logger?.warn?.(`memory-hybrid-bridge[mcp]: ${message}`);
  }

  private resolveServerCommand(): string {
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

  private buildLaunchCandidates(): LaunchConfig[] {
    const command = this.resolveServerCommand();
    const env = {
      ...process.env,
      MEMORY_DB_PATH: this.dbPath,
      TACHI_PROFILE: process.env.TACHI_PROFILE || "runtime",
    } as Record<string, string>;
    const candidates: LaunchConfig[] = [
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

  private async connectWith(launch: LaunchConfig): Promise<Client> {
    const transport = new StdioClientTransport({
      command: launch.command,
      args: launch.args,
      env: launch.env,
      cwd: launch.cwd,
      stderr: "pipe",
    });

    const client = new Client(
      {
        name: "memory-hybrid-bridge",
        version: "0.0.0",
      },
      {},
    );

    await client.connect(transport);
    const listed = await client.listTools();
    const names = new Set(listed.tools.map((tool) => tool.name));
    for (const required of REQUIRED_TOOLS) {
      if (!names.has(required)) {
        await client.close().catch(() => {});
        await transport.close().catch(() => {});
        throw new Error(`required MCP tool missing: ${required}`);
      }
    }

    this.client = client;
    this.transport = transport;
    this.availableTools = names;
    return client;
  }

  private async getClient(): Promise<Client> {
    if (this.client) {
      return this.client;
    }
    if (!this.connecting) {
      this.connecting = (async () => {
        const attempts = this.buildLaunchCandidates();
        let lastError: unknown = null;

        for (let i = 0; i < attempts.length; i++) {
          const launch = attempts[i];
          try {
            const client = await this.connectWith(launch);
            if (i > 0) {
              this.logWarn("connected via compatibility launch (without --global-db)");
            } else {
              this.logInfo(`connected to ${launch.command}`);
            }
            return client;
          } catch (error) {
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

  private async resetConnection(): Promise<void> {
    const client = this.client;
    const transport = this.transport;
    this.client = null;
    this.transport = null;
    this.availableTools.clear();
    await client?.close().catch(() => {});
    await transport?.close().catch(() => {});
  }

  async close(): Promise<void> {
    await this.resetConnection();
  }

  private async callJson<T>(name: string, args: Record<string, unknown> = {}): Promise<T> {
    const client = await this.getClient();
    let result: RawToolResult;
    try {
      result = (await client.callTool({ name, arguments: args })) as RawToolResult;
    } catch (error) {
      await this.resetConnection();
      throw error;
    }

    if (result.isError) {
      throw new Error(extractErrorMessage(result, name));
    }
    return extractJsonPayload<T>(result, name);
  }

  async saveMemory(entry: MemoryEntry): Promise<void> {
    await this.callJson<Record<string, unknown>>("save_memory", {
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

  async getMemory(id: string): Promise<MemoryEntry | undefined> {
    const payload = await this.callJson<unknown>("get_memory", {
      id,
      include_archived: false,
    });
    if (isRecord(payload) && typeof payload.error === "string") {
      return undefined;
    }
    return coerceMemoryEntry(payload);
  }

  async listMemories(limit: number): Promise<MemoryEntry[]> {
    const payload = await this.callJson<unknown>("list_memories", {
      path_prefix: "/",
      limit,
      include_archived: false,
    });
    if (!Array.isArray(payload)) {
      return [];
    }
    return payload.map((row) => coerceMemoryEntry(row)).filter((entry): entry is MemoryEntry => Boolean(entry));
  }

  async searchMemory(query: string, queryVec?: number[], opts?: SearchOptions): Promise<SearchPayload> {
    const payload = await this.callJson<unknown>("search_memory", {
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

    const docs: MemoryEntry[] = [];
    const scores: Record<string, number> = {};
    const scoreBreakdowns: Record<string, HybridScore> = {};

    if (!Array.isArray(payload)) {
      return { docs, scores, scoreBreakdowns };
    }

    for (const row of payload) {
      const entry = coerceMemoryEntry(row);
      if (!entry) {
        continue;
      }
      const scoreRecord = isRecord(row) && isRecord(row.score) ? row.score : null;
      const finalScore = asFiniteNumber(
        scoreRecord?.final ?? scoreRecord?.final_score ?? (isRecord(row) ? row.relevance : undefined),
      );
      const breakdown: HybridScore = {
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

  async recallContext(
    query: string,
    opts?: RecallContextOptions,
  ): Promise<{
    prependContext: string;
    results: Array<{ entry: MemoryEntry; final_score: number }>;
  }> {
    const payload = await this.callJson<unknown>("recall_context", {
      query,
      top_k: opts?.top_k,
      candidate_multiplier: opts?.candidate_multiplier,
      path_prefix: opts?.path_prefix,
      agent_id: opts?.agent_id,
      exclude_topics: opts?.exclude_topics,
      min_score: opts?.min_score,
    });

    let prependContext = "";
    const results: Array<{ entry: MemoryEntry; final_score: number }> = [];

    if (isRecord(payload) && typeof payload.prepend_context === "string") {
      prependContext = payload.prepend_context;
    }

    const rows = isRecord(payload) && Array.isArray(payload.results) ? payload.results : [];
    for (const row of rows) {
      const entry = coerceMemoryEntry(row);
      if (!entry) {
        continue;
      }
      const finalScore = asFiniteNumber(
        (isRecord(row) ? row.relevance : undefined) ??
          (isRecord(row) && isRecord(row.score) ? row.score.final : undefined),
      );
      results.push({ entry, final_score: finalScore });
    }

    return { prependContext, results };
  }

  async captureSession(params: {
    conversation_id: string;
    turn_id: string;
    agent_id: string;
    messages: Array<{ role: string; content: string }>;
    path_prefix?: string;
    scope?: string;
    force?: boolean;
  }): Promise<unknown> {
    return await this.callJson<unknown>("capture_session", params);
  }

  async findSimilarMemory(
    queryVec: number[],
    topK: number,
  ): Promise<Array<{ entry: MemoryEntry; similarity: number }>> {
    if (!this.availableTools.has("find_similar_memory")) {
      throw new Error("find_similar_memory tool is unavailable");
    }

    const payload = await this.callJson<unknown>("find_similar_memory", {
      query_vec: queryVec,
      top_k: topK,
      candidates_per_channel: Math.max(topK, 20),
      include_archived: false,
    });

    if (!Array.isArray(payload)) {
      return [];
    }

    const out: Array<{ entry: MemoryEntry; similarity: number }> = [];
    for (const row of payload) {
      const entry = coerceMemoryEntry(row);
      if (!entry) {
        continue;
      }
      const similarity = asFiniteNumber(
        (isRecord(row) ? row.similarity : undefined) ??
          (isRecord(row) && isRecord(row.score) ? row.score.vector : undefined),
      );
      if (similarity > 0) {
        out.push({ entry, similarity });
      }
    }
    return out;
  }

  async deleteMemory(id: string): Promise<boolean> {
    const payload = await this.callJson<unknown>("delete_memory", { id });
    if (!isRecord(payload)) {
      return false;
    }
    return payload.deleted === true;
  }

  async memoryStats(): Promise<unknown> {
    return await this.callJson<unknown>("memory_stats", {});
  }

  async callTool(toolName: string, args: Record<string, unknown>): Promise<unknown> {
    return await this.callJson<unknown>(toolName, args);
  }
}
