import fs from "node:fs/promises";
import path from "node:path";
import { JsMemoryStore } from "@chaoxlabs/tachi-node";
import type { MemoryEntry } from "./config.js";
import { MemoryMcpClient, type HybridScore } from "./mcp-client.js";

type LoggerLike = {
  info?: (message: string) => void;
  warn?: (message: string) => void;
};

function asFiniteNumber(value: unknown): number {
  const n = typeof value === "number" ? value : Number(value);
  return Number.isFinite(n) ? n : 0;
}

class NapiMemoryStore {
  private readonly store: JsMemoryStore;

  constructor(dbPath: string) {
    this.store = new JsMemoryStore(dbPath);
  }

  upsert(entry: MemoryEntry): void {
    this.store.upsert(JSON.stringify(entry));
  }

  get(id: string): MemoryEntry | undefined {
    const jsonStr = this.store.get(id);
    if (!jsonStr) return undefined;
    return JSON.parse(jsonStr) as MemoryEntry;
  }

  getAll(limit: number): MemoryEntry[] {
    const jsonStr = this.store.getAll(limit);
    if (!jsonStr) return [];
    return JSON.parse(jsonStr) as MemoryEntry[];
  }

  search(
    query: string,
    queryVec?: number[],
    opts?: {
      top_k?: number;
      candidates?: number;
      path_prefix?: string;
      record_access?: boolean;
      weights?: { semantic: number; fts: number; symbolic: number; decay: number };
    },
  ): { docs: MemoryEntry[]; scores: Record<string, number>; scoreBreakdowns: Record<string, HybridScore> } {
    let optionsJson: string | undefined = undefined;
    if (opts || queryVec) {
      const payload: Record<string, unknown> = { ...(opts || {}) };
      if (queryVec) {
        payload.query_vec = queryVec;
      }
      optionsJson = JSON.stringify(payload);
    }

    const resultsJson = this.store.search(query, optionsJson);
    const results = JSON.parse(resultsJson) as Array<{ entry: MemoryEntry; score: unknown }>;

    const docs: MemoryEntry[] = [];
    const scores: Record<string, number> = {};
    const scoreBreakdowns: Record<string, HybridScore> = {};

    for (const r of results) {
      docs.push(r.entry);
      const rawScore = r.score;

      if (rawScore && typeof rawScore === "object") {
        const scoreRecord = rawScore as Record<string, unknown>;
        const breakdown: HybridScore = {
          vector: asFiniteNumber(scoreRecord.vector),
          fts: asFiniteNumber(scoreRecord.fts ?? scoreRecord.lexical),
          symbolic: asFiniteNumber(scoreRecord.symbolic),
          decay: asFiniteNumber(scoreRecord.decay),
          final: asFiniteNumber(scoreRecord.final ?? scoreRecord.final_score),
        };
        scores[r.entry.id] = breakdown.final;
        scoreBreakdowns[r.entry.id] = breakdown;
      } else {
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

  findSimilar(queryVec: number[], topK: number = 5): Array<{ entry: MemoryEntry; similarity: number }> {
    const optionsJson = JSON.stringify({
      query_vec: queryVec,
      top_k: topK,
      candidates: topK,
      record_access: false,
    });
    const resultsJson = this.store.search("", optionsJson);
    const results = JSON.parse(resultsJson) as Array<{
      entry: MemoryEntry;
      score: { vector: number; final: number };
    }>;

    return results
      .filter((r) => r.score.vector > 0)
      .map((r) => ({ entry: r.entry, similarity: r.score.vector }));
  }

  delete(id: string): boolean {
    return this.store.delete(id);
  }

  stats(): unknown {
    return JSON.parse(this.store.stats(false));
  }
}

export class MemoryStore {
  private readonly napiStore: NapiMemoryStore;
  private readonly mcpClient: MemoryMcpClient | null;
  private readonly preferredBackend: "mcp" | "napi";
  private mcpFailedAt: number | null = null;
  private static readonly MCP_RETRY_AFTER_MS = 30_000;

  constructor(
    dbPath: string,
    private readonly logger?: LoggerLike,
  ) {
    this.napiStore = new NapiMemoryStore(dbPath);
    const backendRaw = (process.env.OPENCLAW_MEMORY_BACKEND || "mcp").trim().toLowerCase();
    this.preferredBackend = backendRaw === "napi" ? "napi" : "mcp";
    this.mcpClient = this.preferredBackend === "mcp" ? new MemoryMcpClient(dbPath, logger) : null;
  }

  private isMcpAvailable(): boolean {
    if (this.mcpFailedAt === null) return true;
    if (Date.now() - this.mcpFailedAt > MemoryStore.MCP_RETRY_AFTER_MS) {
      this.mcpFailedAt = null;
      this.logger?.info?.("memory-hybrid-bridge: MCP backend retry window reached, attempting reconnect");
      return true;
    }
    return false;
  }

  private async withBackend<T>(
    operation: string,
    mcpRun: () => Promise<T>,
    napiRun: () => T | Promise<T>,
  ): Promise<T> {
    if (this.preferredBackend === "napi" || !this.isMcpAvailable() || !this.mcpClient) {
      return await napiRun();
    }

    try {
      return await mcpRun();
    } catch (error) {
      this.mcpFailedAt = Date.now();
      this.logger?.warn?.(
        `memory-hybrid-bridge: MCP backend failed during ${operation}, falling back to NAPI (retry in ${MemoryStore.MCP_RETRY_AFTER_MS / 1000}s): ${String(error)}`,
      );
      return await napiRun();
    }
  }

  async close(): Promise<void> {
    await this.mcpClient?.close();
  }

  async upsert(entry: MemoryEntry): Promise<void> {
    await this.withBackend(
      "upsert",
      async () => {
        await this.mcpClient!.saveMemory(entry);
      },
      () => {
        this.napiStore.upsert(entry);
      },
    );
  }

  async get(id: string): Promise<MemoryEntry | undefined> {
    return await this.withBackend(
      "get",
      async () => await this.mcpClient!.getMemory(id),
      () => this.napiStore.get(id),
    );
  }

  async getAll(limit: number): Promise<MemoryEntry[]> {
    return await this.withBackend(
      "getAll",
      async () => await this.mcpClient!.listMemories(limit),
      () => this.napiStore.getAll(limit),
    );
  }

  async search(
    query: string,
    queryVec?: number[],
    opts?: {
      top_k?: number;
      candidates?: number;
      path_prefix?: string;
      record_access?: boolean;
      weights?: { semantic: number; fts: number; symbolic: number; decay: number };
    },
  ): Promise<{ docs: MemoryEntry[]; scores: Record<string, number>; scoreBreakdowns: Record<string, HybridScore> }> {
    return await this.withBackend(
      "search",
      async () => await this.mcpClient!.searchMemory(query, queryVec, opts),
      () => this.napiStore.search(query, queryVec, opts),
    );
  }

  async findSimilar(
    queryVec: number[],
    topK: number = 5,
  ): Promise<Array<{ entry: MemoryEntry; similarity: number }>> {
    return await this.withBackend(
      "findSimilar",
      async () => await this.mcpClient!.findSimilarMemory(queryVec, topK),
      () => this.napiStore.findSimilar(queryVec, topK),
    );
  }

  async delete(id: string): Promise<boolean> {
    return await this.withBackend(
      "delete",
      async () => await this.mcpClient!.deleteMemory(id),
      () => this.napiStore.delete(id),
    );
  }

  async stats(): Promise<unknown> {
    return await this.withBackend(
      "stats",
      async () => await this.mcpClient!.memoryStats(),
      () => this.napiStore.stats(),
    );
  }
}

// Auto-migration wrapper
export async function getStore(
  dbPath: string,
  legacyShadowPath?: string,
  logger?: any,
): Promise<MemoryStore> {
  const needsMigration =
    legacyShadowPath &&
    (await fs
      .stat(legacyShadowPath)
      .then((s) => s.size > 0)
      .catch(() => false)) &&
    !(await fs.stat(dbPath).catch(() => false));

  await fs.mkdir(path.dirname(dbPath), { recursive: true });
  const store = new MemoryStore(dbPath, logger);

  if (needsMigration) {
    logger?.info("memory-hybrid-bridge: initiating SQLite migration from JSONL");
    try {
      const raw = await fs.readFile(legacyShadowPath!, "utf8");
      const lines = raw
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean);

      const toMigrate: MemoryEntry[] = [];
      for (const line of lines) {
        try {
          const e = JSON.parse(line) as MemoryEntry;
          if (e.id || (e as any).entry_id) {
            // Remap old 'entry_id' -> 'id', 'lossless_restatement' -> 'text'
            // Rust serde alias handles decoding but we rewrite here to clean
            if (!e.path) e.path = "/openclaw/legacy";
            if (!e.summary) e.summary = (e.text || (e as any).lossless_restatement || "").substring(0, 100);
            if (!e.importance) e.importance = 0.8;
            toMigrate.push(e);
          }
        } catch (e) { }
      }

      if (toMigrate.length > 0) {
        // Just sequentially insert mapping old keys
        for (const e of toMigrate) {
          await store.upsert(e);
        }
        logger?.info(
          `memory-hybrid-bridge: successfully migrated ${toMigrate.length} memories to Rust SQLite`,
        );
        // Backup the old file so it won't be reused
        await fs
          .rename(legacyShadowPath!, `${legacyShadowPath}.migrated-${Date.now()}`)
          .catch(() => { });
      }
    } catch (e) {
      logger?.warn(`memory-hybrid-bridge: migration error: ${String(e)}`);
    }
  }

  return store;
}
