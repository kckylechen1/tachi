/**
 * Real-data recall test: loads actual memory.db → imports into Rust engine → compares search results
 * Adapts the old schema (text, topic, keywords, scope, created_at) to the new schema.
 */
const { JsMemoryStore } = require('./index.js');
const Database = require('/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/node_modules/better-sqlite3');
const path = require('path');
const fs = require('fs');

const REAL_DB = '/Users/kckylechen/.gemini/antigravity/memory.db';
const RUST_DB = path.join(__dirname, '_real_rust.db');
if (fs.existsSync(RUST_DB)) fs.unlinkSync(RUST_DB);

// ── 1. Load all entries from real DB ──────────────────────────────────────────
const oldDb = new Database(REAL_DB, { readonly: true });
const allEntries = oldDb.prepare('SELECT * FROM memories ORDER BY created_at DESC').all();
console.log(`\n📦 Loaded ${allEntries.length} memories from real DB\n`);

// Show some samples
console.log('  Sample entries:');
allEntries.slice(0, 3).forEach(e => {
    const kw = e.keywords || '[]';
    console.log(`    [${e.id.slice(0, 8)}] path=${e.path}  topic=${e.topic}  kw=${kw.slice(0, 50)}`);
    console.log(`             "${(e.text || '').slice(0, 80)}"`);
});
console.log('  ...\n');

// ── 2. Import into Rust store ─────────────────────────────────────────────────
const rustStore = new JsMemoryStore(RUST_DB);
let imported = 0;
for (const row of allEntries) {
    try {
        let kwArray = [];
        try { kwArray = JSON.parse(row.keywords || '[]'); } catch { }
        if (typeof kwArray === 'string') kwArray = [kwArray];

        const entry = {
            id: row.id,
            path: row.path || '/',
            summary: row.summary || (row.text || '').slice(0, 30),
            text: row.text || '',
            importance: row.importance || 0.7,
            event_time: row.created_at || new Date().toISOString(),
            record_time: row.created_at || new Date().toISOString(),
            category: row.scope || 'fact',
            access_count: 0,
            last_access: null,
            metadata: {
                keywords: kwArray,
                entities: [],
                topic: row.topic || '',
                source: row.source || 'manual',
            },
        };
        rustStore.upsert(JSON.stringify(entry));
        imported++;
    } catch (e) {
        console.log(`  ⚠️ Skip ${row.id}: ${e.message}`);
    }
}
console.log(`✅ Imported ${imported}/${allEntries.length} entries into Rust store\n`);

// ── 3. Old-style FTS search on original DB ────────────────────────────────────
function oldSearch(query, topK = 5) {
    const safe = query.replace(/[^\w\s\u4e00-\u9fff]/g, '').trim();
    if (!safe) return [];
    try {
        return oldDb.prepare(
            'SELECT id, -bm25(memories_fts) AS score FROM memories_fts WHERE memories_fts MATCH ? ORDER BY bm25(memories_fts) LIMIT ?'
        ).all(safe, topK);
    } catch { return []; }
}

function getPreview(id) {
    const e = allEntries.find(e => e.id === id);
    if (!e) return '(not found)';
    return (e.text || e.summary || '').slice(0, 70).replace(/\n/g, ' ');
}

// ── 4. Test queries ───────────────────────────────────────────────────────────
const QUERIES = [
    'OpenClaw 插件',
    '记忆系统',
    'Rust 重写',
    'memory plugin',
    '配置变更',
    '模型选择',
    'API密钥',
    '待办',
    '用户偏好',
    '数据库',
    'FTS5 分词',
    '代理框架',
    'DragonFly',
    'Voyage',
];

console.log('═══════════════════════════════════════════════════════════════════════════');
console.log('  Real-Data Recall: Rust (simple tokenizer) vs Old (unicode61)');
console.log(`  Dataset: ${allEntries.length} real memories`);
console.log('═══════════════════════════════════════════════════════════════════════════\n');

let rustTotal = 0, oldTotal = 0;

QUERIES.forEach((q, i) => {
    const rustResults = JSON.parse(rustStore.search(q, 5));
    const oldResults = oldSearch(q, 5);

    console.log(`  ${String(i + 1).padStart(2)}. 🔍 "${q}"    Rust: ${rustResults.length} hits | Old: ${oldResults.length} hits`);

    const maxShow = Math.max(rustResults.length, oldResults.length, 1);
    for (let j = 0; j < Math.min(maxShow, 3); j++) {
        const rr = rustResults[j];
        const or = oldResults[j];
        const rustLine = rr ? `[${rr.score.final.toFixed(3)}] ${getPreview(rr.entry.id)}` : '';
        const oldLine = or ? `[${or.score.toFixed(3)}] ${getPreview(or.id)}` : '';

        if (rustLine) console.log(`       Rust ${j + 1}. ${rustLine}`);
        if (oldLine) console.log(`       Old  ${j + 1}. ${oldLine}`);
    }

    rustTotal += rustResults.length;
    oldTotal += oldResults.length;
    console.log('');
});

console.log('═══════════════════════════════════════════════════════════════════════════');
console.log(`  Total results: Rust ${rustTotal} | Old ${oldTotal}  (across ${QUERIES.length} queries)`);
console.log(`  Avg per query: Rust ${(rustTotal / QUERIES.length).toFixed(1)} | Old ${(oldTotal / QUERIES.length).toFixed(1)}`);
console.log('═══════════════════════════════════════════════════════════════════════════\n');

// Cleanup
fs.unlinkSync(RUST_DB);
oldDb.close();
