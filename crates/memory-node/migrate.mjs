import fs from 'fs/promises';
import { JsMemoryStore } from './index.js';

async function migrate() {
    console.log('Starting migration...');
    const legacyShadowPath = '/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/data/shadow-store.jsonl';
    const newDbPath = '/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/data/memory.db';

    // 1. Rename existing DB to force fresh creation by the new schema if it exists
    try {
        await fs.rename(newDbPath, newDbPath + '.old');
        console.log(`Backed up old DB to ${newDbPath}.old`);
    } catch (e) {
        // ignore if not exists
    }

    // 2. Initialize new NAPI store
    const store = new JsMemoryStore(newDbPath);
    console.log(`Initialized new Rust-backed memory store at ${newDbPath}`);

    // 3. Migrate from JSONL shadow store
    try {
        const raw = await fs.readFile(legacyShadowPath, 'utf8');
        const lines = raw.split('\n').map(l => l.trim()).filter(Boolean);

        let count = 0;
        for (const line of lines) {
            try {
                const oldEntry = JSON.parse(line);
                if (oldEntry.entry_id || oldEntry.id) {
                    const newEntry = {
                        id: oldEntry.id || oldEntry.entry_id,
                        path: oldEntry.path || '/openclaw/legacy',
                        summary: oldEntry.summary || (oldEntry.text || oldEntry.lossless_restatement || '').substring(0, 100),
                        text: oldEntry.text || oldEntry.lossless_restatement || '',
                        importance: oldEntry.importance ?? 0.8,
                        timestamp: oldEntry.timestamp || oldEntry.created_at || new Date().toISOString(),
                        category: oldEntry.category || 'other',
                        topic: oldEntry.topic || '',
                        keywords: oldEntry.keywords || [],
                        persons: oldEntry.persons || [],
                        entities: oldEntry.entities || [],
                        location: oldEntry.location || '',
                        source: oldEntry.source || 'migration',
                        scope: oldEntry.scope || 'general',
                        vector: oldEntry.vector,
                        metadata: {
                            source_refs: oldEntry.source_refs || []
                        }
                    };
                    store.upsert(JSON.stringify(newEntry));
                    count++;
                }
            } catch (err) {
                console.error('Failed to parse line:', line, err);
            }
        }
        console.log(`Successfully migrated ${count} entries from ${legacyShadowPath}`);
    } catch (e) {
        console.warn(`Migration from shadow-store skipped: ${e.message}`);
    }
    console.log('Migration complete.');
}

migrate().catch(console.error);
