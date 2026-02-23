import fs from "node:fs/promises";
import path from "node:path";
import { JsMemoryStore } from "@memory-core/node";
import type { BridgeConfig, MemoryEntry } from "./config.js";

export class MemoryStore {
  private store: JsMemoryStore;

  constructor(dbPath: string) {
    this.store = new JsMemoryStore(dbPath);
  }

  upsert(entry: MemoryEntry) {
    // Rust bindings expect JSON strings.
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

  /**
   * Delegates hybrid search to the Rust core.
   * `queryVec` is passed to Rust, which merges FTS + Vec + Symbolic scores.
   * Note: The Rust core expects the topK, pathPrefix, weights, etc. options
   * to be passed as a JSON string to keep the NAPI boundary simple.
   */
  search(
    query: string,
    queryVec?: number[],
    opts?: {
      top_k?: number;
      candidates?: number;
      path_prefix?: string;
      record_access?: boolean;
      weights?: { semantic: number; fts: number; symbolic: number; decay: number };
    }
  ): { docs: MemoryEntry[]; scores: Record<string, number> } {
    let optionsJson: string | undefined = undefined;
    if (opts || queryVec) {
      const payload: any = { ...opts };
      if (queryVec) {
        payload.query_vec = queryVec;
      }
      optionsJson = JSON.stringify(payload);
    }

    const resultsJson = this.store.search(query, optionsJson);
    const results = JSON.parse(resultsJson) as Array<{ entry: MemoryEntry; score: any }>;

    const docs: MemoryEntry[] = [];
    const scores: Record<string, number> = {};

    for (const r of results) {
      docs.push(r.entry);
      scores[r.entry.id] = r.score.final; // Or just r.score
    }

    return { docs, scores };
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
  const store = new MemoryStore(dbPath);

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
          store.upsert(e);
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
