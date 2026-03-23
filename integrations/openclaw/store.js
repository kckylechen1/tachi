import fs from "node:fs/promises";
import path from "node:path";
import { JsMemoryStore } from "@chaoxlabs/tachi-node";
export class MemoryStore {
    store;
    constructor(dbPath) {
        this.store = new JsMemoryStore(dbPath);
    }
    upsert(entry) {
        // Rust bindings expect JSON strings.
        this.store.upsert(JSON.stringify(entry));
    }
    get(id) {
        const jsonStr = this.store.get(id);
        if (!jsonStr)
            return undefined;
        return JSON.parse(jsonStr);
    }
    getAll(limit) {
        const jsonStr = this.store.getAll(limit);
        if (!jsonStr)
            return [];
        return JSON.parse(jsonStr);
    }
    /**
     * Delegates hybrid search to the Rust core.
     * `queryVec` is passed to Rust, which merges FTS + Vec + Symbolic scores.
     * Note: The Rust core expects the topK, pathPrefix, weights, etc. options
     * to be passed as a JSON string to keep the NAPI boundary simple.
     */
    search(query, queryVec, opts) {
        let optionsJson = undefined;
        if (opts || queryVec) {
            const payload = { ...opts };
            if (queryVec) {
                payload.query_vec = queryVec;
            }
            optionsJson = JSON.stringify(payload);
        }
        const resultsJson = this.store.search(query, optionsJson);
        const results = JSON.parse(resultsJson);
        const docs = [];
        const scores = {};
        for (const r of results) {
            docs.push(r.entry);
            scores[r.entry.id] = r.score.final; // Or just r.score
        }
        return { docs, scores };
    }
    /**
     * Find entries similar to the given vector using Rust's sqlite-vec KNN.
     * Returns entries with their raw cosine similarity (from the vector channel).
     * Used for dedup/merge decisions — no FTS or symbolic scoring involved.
     */
    findSimilar(queryVec, topK = 5) {
        const optionsJson = JSON.stringify({
            query_vec: queryVec,
            top_k: topK,
            candidates: topK,
            record_access: false,
        });
        // Empty query string → FTS returns nothing, only vector channel fires
        const resultsJson = this.store.search("", optionsJson);
        const results = JSON.parse(resultsJson);
        return results
            .filter(r => r.score.vector > 0)
            .map(r => ({ entry: r.entry, similarity: r.score.vector }));
    }
    /**
     * Delete a memory entry by ID (used after merge to remove the old entry).
     * Falls back silently if the entry doesn't exist.
     */
    delete(id) {
        // Rust binding doesn't expose delete yet; upsert with empty text as tombstone
        // TODO: add proper delete to Rust binding
        this.store.upsert(JSON.stringify({
            id,
            text: "",
            summary: "[merged]",
            keywords: [],
            timestamp: new Date().toISOString(),
            location: "",
            persons: [],
            entities: [],
            topic: "",
            scope: "general",
            path: "/openclaw/merged",
            category: "other",
            importance: 0,
            access_count: 0,
            last_access: null,
            metadata: { source_refs: [], merged: true },
        }));
    }
}
// Auto-migration wrapper
export async function getStore(dbPath, legacyShadowPath, logger) {
    const needsMigration = legacyShadowPath &&
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
            const raw = await fs.readFile(legacyShadowPath, "utf8");
            const lines = raw
                .split("\n")
                .map((l) => l.trim())
                .filter(Boolean);
            const toMigrate = [];
            for (const line of lines) {
                try {
                    const e = JSON.parse(line);
                    if (e.id || e.entry_id) {
                        // Remap old 'entry_id' -> 'id', 'lossless_restatement' -> 'text'
                        // Rust serde alias handles decoding but we rewrite here to clean
                        if (!e.path)
                            e.path = "/openclaw/legacy";
                        if (!e.summary)
                            e.summary = (e.text || e.lossless_restatement || "").substring(0, 100);
                        if (!e.importance)
                            e.importance = 0.8;
                        toMigrate.push(e);
                    }
                }
                catch (e) { }
            }
            if (toMigrate.length > 0) {
                // Just sequentially insert mapping old keys
                for (const e of toMigrate) {
                    store.upsert(e);
                }
                logger?.info(`memory-hybrid-bridge: successfully migrated ${toMigrate.length} memories to Rust SQLite`);
                // Backup the old file so it won't be reused
                await fs
                    .rename(legacyShadowPath, `${legacyShadowPath}.migrated-${Date.now()}`)
                    .catch(() => { });
            }
        }
        catch (e) {
            logger?.warn(`memory-hybrid-bridge: migration error: ${String(e)}`);
        }
    }
    return store;
}
