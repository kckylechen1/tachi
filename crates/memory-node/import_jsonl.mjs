import fs from 'fs';
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { JsMemoryStore } = require('./index.js');

const JSONL_PATH = '/Users/kckylechen/.openclaw/workspace/extensions/memory-hybrid-bridge/data/shadow-store.jsonl';
const DB_PATH = '/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/data/memory.db';

const store = new JsMemoryStore(DB_PATH);
const lines = fs.readFileSync(JSONL_PATH, 'utf8').split('\n').filter(Boolean);

console.log(`Reading ${lines.length} entries from ${JSONL_PATH}`);

let imported = 0, skipped = 0, errors = 0;

for (const line of lines) {
    try {
        const old = JSON.parse(line);
        // Map old fields to new unified schema
        const entry = {
            id: old.id || old.entry_id,
            path: old.path || '/openclaw/legacy',
            summary: old.summary || (old.text || old.lossless_restatement || '').substring(0, 100),
            text: old.text || old.lossless_restatement || '',
            importance: old.importance ?? 0.8,
            timestamp: old.timestamp || old.created_at || new Date().toISOString(),
            category: old.category || 'other',
            topic: old.topic || '',
            keywords: old.keywords || [],
            persons: old.persons || [],
            entities: old.entities || [],
            location: old.location || '',
            source: old.source || 'migration',
            scope: old.scope || 'general',
            access_count: 0,
            last_access: null,
            // Skip vector to avoid memories_vec table issues
            metadata: { source_refs: old.source_refs || [] }
        };

        if (!entry.id || !entry.text) {
            skipped++;
            continue;
        }

        store.upsert(JSON.stringify(entry));
        imported++;
    } catch (err) {
        errors++;
        if (errors <= 3) console.error(`Error: ${err.message}`);
    }
}

console.log(`✅ Imported: ${imported}, Skipped: ${skipped}, Errors: ${errors}`);

// Verify total
const allJson = store.getAll(999);
const all = JSON.parse(allJson);
console.log(`Total entries in DB now: ${all.length}`);
