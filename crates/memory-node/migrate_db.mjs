import fs from 'fs';
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { JsMemoryStore } = require('./index.js');

function migrateFromJson(jsonPath, dbPath, label) {
    const raw = fs.readFileSync(jsonPath, 'utf8');
    const rows = JSON.parse(raw);
    console.log(`[${label}] Read ${rows.length} entries from ${jsonPath}`);

    const store = new JsMemoryStore(dbPath);
    let count = 0, errors = 0;

    for (const row of rows) {
        try {
            const metadata = typeof row.metadata === 'string' ? JSON.parse(row.metadata) : (row.metadata || {});
            const vectorRaw = row.vector;
            let vector = undefined;
            if (vectorRaw && vectorRaw !== 'null') {
                vector = typeof vectorRaw === 'string' ? JSON.parse(vectorRaw) : vectorRaw;
            }

            const entry = {
                id: row.id,
                path: row.path || '/openclaw/legacy',
                summary: row.summary || '',
                text: row.text || '',
                importance: row.importance ?? 0.8,
                timestamp: row.created_at || new Date().toISOString(),
                category: metadata.category || 'other',
                topic: metadata.topic || '',
                keywords: metadata.keywords || [],
                persons: metadata.persons || [],
                entities: metadata.entities || [],
                location: metadata.location || '',
                source: 'migration',
                scope: metadata.scope || 'general',
                access_count: 0,
                last_access: null,
                vector: vector,
                metadata: { source_refs: metadata.source_refs || [] }
            };
            store.upsert(JSON.stringify(entry));
            count++;
        } catch (err) {
            errors++;
            console.error(`[${label}] Failed: ${row.id} - ${err.message}`);
        }
    }

    console.log(`[${label}] ✅ Migrated ${count} entries (${errors} errors)`);

    // Quick verify
    const allJson = store.getAll(5);
    const all = JSON.parse(allJson);
    console.log(`[${label}] Verification: showing first ${all.length} entries`);
    for (const e of all) {
        console.log(`  - ${e.id}: ${(e.summary || e.text).substring(0, 60)}`);
    }
}

// Main agent (yaya)
migrateFromJson(
    '/tmp/main_memories.json',
    '/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/data/memory.db',
    'main/yaya'
);

// Jayne agent
migrateFromJson(
    '/tmp/jayne_memories.json',
    '/Users/kckylechen/.openclaw/agents/jayne/memory/memory.db',
    'jayne'
);
