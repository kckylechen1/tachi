/**
 * Accuracy Test: Rust NAPI vs better-sqlite3
 *
 * Tests search relevance with ground-truth expected results.
 * Each test case has a query + expected document IDs that should appear in results.
 */

const { JsMemoryStore } = require('./index.js');
const Database = require('/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/node_modules/better-sqlite3');
const path = require('path');
const fs = require('fs');

// ── Test Data ─────────────────────────────────────────────────────────────────
const DOCUMENTS = [
    { id: 'd01', text: 'Rust是一门系统编程语言，注重安全性和性能', keywords: ['rust', '系统编程', '安全'] },
    { id: 'd02', text: 'Python适合做数据分析和机器学习', keywords: ['python', '数据分析', '机器学习'] },
    { id: 'd03', text: 'React是Facebook开发的前端框架，使用JSX语法', keywords: ['react', '前端', 'jsx'] },
    { id: 'd04', text: '记忆系统使用混合检索来提高搜索质量和召回率', keywords: ['记忆', '混合检索', '召回'] },
    { id: 'd05', text: 'Docker容器化技术可以实现应用的快速部署', keywords: ['docker', '容器', '部署'] },
    { id: 'd06', text: 'SQLite是轻量级嵌入式数据库，支持FTS5全文检索', keywords: ['sqlite', '数据库', 'fts5'] },
    { id: 'd07', text: 'OpenClaw是一个开源的AI代理框架', keywords: ['openclaw', 'ai', '代理'] },
    { id: 'd08', text: '向量数据库用于存储和检索高维向量嵌入', keywords: ['向量', '数据库', '嵌入'] },
    { id: 'd09', text: 'WebSocket实现了浏览器和服务器之间的全双工实时通信', keywords: ['websocket', '实时', '通信'] },
    { id: 'd10', text: 'Kubernetes集群管理和容器编排平台', keywords: ['kubernetes', '集群', '容器'] },
    { id: 'd11', text: '周杰伦是华语乐坛的天王巨星', keywords: ['周杰伦', '音乐', '华语'] },
    { id: 'd12', text: 'TypeScript是JavaScript的超集，增加了静态类型检查', keywords: ['typescript', 'javascript', '类型'] },
    { id: 'd13', text: 'Redis是高性能的内存缓存数据库', keywords: ['redis', '缓存', '内存'] },
    { id: 'd14', text: 'GraphQL是一种API查询语言，比REST更灵活', keywords: ['graphql', 'api', '查询'] },
    { id: 'd15', text: 'PyO3可以让Python调用Rust编写的原生模块', keywords: ['pyo3', 'python', 'rust'] },
];

// Query → expected top results (ordered by relevance)
const TEST_CASES = [
    // ── Chinese queries ───────────────────────────────────────────────────────
    { query: '记忆检索', expected: ['d04'], desc: '中文：记忆+检索组合' },
    { query: '容器部署', expected: ['d05', 'd10'], desc: '中文：容器相关（两条）' },
    { query: '数据库', expected: ['d06', 'd08', 'd13'], desc: '中文：数据库（三条相关）' },
    { query: '系统编程语言', expected: ['d01'], desc: '中文：系统编程' },
    { query: '周杰伦', expected: ['d11'], desc: '中文：人名' },
    { query: '实时通信', expected: ['d09'], desc: '中文：WebSocket' },

    // ── English queries ───────────────────────────────────────────────────────
    { query: 'rust performance', expected: ['d01', 'd15'], desc: 'EN: Rust相关' },
    { query: 'python learning', expected: ['d02', 'd15'], desc: 'EN: Python相关' },
    { query: 'docker container', expected: ['d05', 'd10'], desc: 'EN: Docker/K8s' },
    { query: 'database', expected: ['d06', 'd08', 'd13'], desc: 'EN: database' },
    { query: 'API query', expected: ['d14'], desc: 'EN: GraphQL API' },

    // ── Cross-language / tricky ───────────────────────────────────────────────
    { query: 'Rust Python调用', expected: ['d15'], desc: '混合：PyO3' },
    { query: 'AI代理', expected: ['d07'], desc: '中文+英文混合' },
    { query: 'FTS5全文', expected: ['d06'], desc: '术语+中文' },
];

// ── Setup stores ──────────────────────────────────────────────────────────────
const RUST_DB = path.join(__dirname, '_acc_rust.db');
const OLD_DB = path.join(__dirname, '_acc_old.db');
[RUST_DB, OLD_DB].forEach(p => { if (fs.existsSync(p)) fs.unlinkSync(p); });

// Rust store
const rustStore = new JsMemoryStore(RUST_DB);

// Old store (better-sqlite3)
class OldStore {
    constructor(dbPath) {
        this.db = new Database(dbPath);
        this.db.pragma('journal_mode = WAL');
        this.db.exec(`
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY, path TEXT, summary TEXT, text TEXT,
                importance REAL, event_time TEXT, record_time TEXT,
                category TEXT, access_count INTEGER, last_access TEXT, metadata TEXT
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                id UNINDEXED, path, summary, text, keywords, entities
            );
        `);
    }
    upsert(entry) {
        const meta = JSON.stringify(entry.metadata);
        this.db.prepare(`INSERT OR REPLACE INTO memories VALUES (?,?,?,?,?,?,?,?,?,?,?)`)
            .run(entry.id, entry.path, entry.summary, entry.text, entry.importance,
                entry.event_time, entry.record_time, entry.category, entry.access_count, entry.last_access, meta);
        this.db.prepare('DELETE FROM memories_fts WHERE id = ?').run(entry.id);
        this.db.prepare('INSERT INTO memories_fts(id,path,summary,text,keywords,entities) VALUES (?,?,?,?,?,?)')
            .run(entry.id, entry.path, entry.summary, entry.text,
                (entry.metadata.keywords || []).join(' '), (entry.metadata.entities || []).join(' '));
    }
    search(query, topK = 6) {
        const safe = query.replace(/[^\w\s\u4e00-\u9fff]/g, '').trim();
        if (!safe) return [];
        try {
            return this.db.prepare('SELECT id, -bm25(memories_fts) AS score FROM memories_fts WHERE memories_fts MATCH ? ORDER BY bm25(memories_fts) LIMIT ?')
                .all(safe, topK).map(r => r.id);
        } catch { return []; }
    }
}
const oldStore = new OldStore(OLD_DB);

// Insert same data into both stores
DOCUMENTS.forEach(doc => {
    const entry = {
        id: doc.id,
        path: '/test',
        summary: doc.text.slice(0, 30),
        text: doc.text,
        importance: 0.7,
        event_time: new Date().toISOString(),
        record_time: new Date().toISOString(),
        category: 'fact',
        access_count: 0,
        last_access: null,
        metadata: { keywords: doc.keywords, entities: [] },
    };
    rustStore.upsert(JSON.stringify(entry));
    oldStore.upsert(entry);
});

// ── Run tests ─────────────────────────────────────────────────────────────────
console.log('═══════════════════════════════════════════════════════════════════════');
console.log('  Search Accuracy Test: Rust NAPI vs better-sqlite3');
console.log('═══════════════════════════════════════════════════════════════════════\n');

let rustHits = 0, oldHits = 0, totalExpected = 0;

TEST_CASES.forEach((tc, i) => {
    const rustResults = JSON.parse(rustStore.search(tc.query, 6)).map(r => r.entry.id);
    const oldResults = oldStore.search(tc.query, 6);

    // Check recall: how many expected IDs are in the results?
    const rustRecall = tc.expected.filter(e => rustResults.includes(e));
    const oldRecall = tc.expected.filter(e => oldResults.includes(e));

    const rustOK = rustRecall.length === tc.expected.length;
    const oldOK = oldRecall.length === tc.expected.length;

    rustHits += rustRecall.length;
    oldHits += oldRecall.length;
    totalExpected += tc.expected.length;

    const rustIcon = rustOK ? '✅' : (rustRecall.length > 0 ? '🟡' : '❌');
    const oldIcon = oldOK ? '✅' : (oldRecall.length > 0 ? '🟡' : '❌');

    console.log(`  ${String(i + 1).padStart(2)}. "${tc.query}"`);
    console.log(`      ${tc.desc}`);
    console.log(`      期望: [${tc.expected.join(', ')}]`);
    console.log(`      Rust ${rustIcon}  → [${rustResults.slice(0, 4).join(', ')}]  recall: ${rustRecall.length}/${tc.expected.length}`);
    console.log(`      Old  ${oldIcon}  → [${oldResults.slice(0, 4).join(', ')}]  recall: ${oldRecall.length}/${tc.expected.length}`);
    console.log('');
});

// ── Summary ───────────────────────────────────────────────────────────────────
const rustPct = (rustHits / totalExpected * 100).toFixed(1);
const oldPct = (oldHits / totalExpected * 100).toFixed(1);

console.log('═══════════════════════════════════════════════════════════════════════');
console.log(`  Total Recall: Rust ${rustHits}/${totalExpected} (${rustPct}%)  |  Old ${oldHits}/${totalExpected} (${oldPct}%)`);
console.log('═══════════════════════════════════════════════════════════════════════');

// Cleanup
[RUST_DB, OLD_DB].forEach(p => { if (fs.existsSync(p)) fs.unlinkSync(p); });
