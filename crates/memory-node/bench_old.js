/**
 * Performance Benchmark: OLD better-sqlite3 implementation
 * Mirrors bench.js logic for fair comparison with Rust NAPI.
 */

const Database = require('/Users/kckylechen/.openclaw/local-plugins/extensions/memory-hybrid-bridge/node_modules/better-sqlite3');
const path = require('path');
const fs = require('fs');

const NUM_ENTRIES = 1000;
const SEARCH_ITERATIONS = 200;
const DB_PATH = path.join(__dirname, '_bench_old.db');

// ── Helpers ───────────────────────────────────────────────────────────────────
function generateEntry(i) {
    const categories = ['fact', 'decision', 'experience', 'preference'];
    const topics = [
        'Rust系统编程语言性能优化',
        'Python机器学习框架选型',
        'TypeScript前端开发最佳实践',
        'SQLite数据库性能调优与索引',
        'React组件设计模式和Hook',
        'Docker容器化部署方案',
        'GraphQL API设计与实现',
        'Kubernetes集群管理运维',
        'Redis缓存策略与淘汰机制',
        'WebSocket实时通信架构',
    ];
    const text = topics[i % topics.length] + ` — 详细记录第${i}条，包含技术方案和决策过程。` +
        `这是一段较长的文本用于测试FTS5的索引和检索性能。每条记忆都有不同的关键词组合。`;
    return {
        id: `bench-${String(i).padStart(5, '0')}`,
        path: `/bench/topic-${i % 5}`,
        summary: text.slice(0, 30),
        text,
        importance: 0.3 + Math.random() * 0.7,
        event_time: new Date(Date.now() - Math.random() * 86400000 * 90).toISOString(),
        record_time: new Date().toISOString(),
        category: categories[i % categories.length],
        access_count: Math.floor(Math.random() * 10),
        last_access: Math.random() > 0.5 ? new Date().toISOString() : null,
        metadata: {
            keywords: [topics[i % topics.length].slice(0, 4), `kw-${i % 20}`, 'benchmark'],
            entities: [`entity-${i % 15}`],
        },
    };
}

function percentile(arr, p) {
    const sorted = [...arr].sort((a, b) => a - b);
    return sorted[Math.max(0, Math.ceil(sorted.length * p / 100) - 1)];
}

function stats(arr) {
    const sum = arr.reduce((a, b) => a + b, 0);
    return {
        mean: (sum / arr.length).toFixed(3),
        p50: percentile(arr, 50).toFixed(3),
        p95: percentile(arr, 95).toFixed(3),
        p99: percentile(arr, 99).toFixed(3),
    };
}

// ── Old-style store (mirrors store.ts logic) ──────────────────────────────────
class OldStore {
    constructor(dbPath) {
        this.db = new Database(dbPath);
        this.db.pragma('journal_mode = WAL');
        this.db.pragma('cache_size = -16000');
        this.db.exec(`
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY, path TEXT NOT NULL DEFAULT '/',
                summary TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '',
                importance REAL NOT NULL DEFAULT 0.7,
                event_time TEXT NOT NULL, record_time TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'fact',
                access_count INTEGER NOT NULL DEFAULT 0,
                last_access TEXT, metadata TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_path ON memories(path);
            CREATE INDEX IF NOT EXISTS idx_importance ON memories(importance DESC);
            CREATE INDEX IF NOT EXISTS idx_record_time ON memories(record_time DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                id UNINDEXED, path, summary, text, keywords, entities
            );
        `);

        this._upsertStmt = this.db.prepare(`
            INSERT INTO memories (id,path,summary,text,importance,event_time,record_time,category,access_count,last_access,metadata)
            VALUES (?,?,?,?,?,?,?,?,?,?,?)
            ON CONFLICT(id) DO UPDATE SET path=excluded.path, summary=excluded.summary, text=excluded.text,
                importance=excluded.importance, event_time=excluded.event_time, record_time=excluded.record_time,
                category=excluded.category, access_count=excluded.access_count, last_access=excluded.last_access,
                metadata=excluded.metadata
        `);
        this._ftsDelete = this.db.prepare('DELETE FROM memories_fts WHERE id = ?');
        this._ftsInsert = this.db.prepare(
            'INSERT INTO memories_fts(id,path,summary,text,keywords,entities) VALUES (?,?,?,?,?,?)'
        );
        this._ftsSearch = this.db.prepare(
            'SELECT id, -bm25(memories_fts) AS score FROM memories_fts WHERE memories_fts MATCH ? ORDER BY bm25(memories_fts) LIMIT ?'
        );
        this._getById = this.db.prepare('SELECT * FROM memories WHERE id = ?');
        this._fetchByIds = null; // dynamic
    }

    upsert(entry) {
        const meta = JSON.stringify(entry.metadata);
        const kws = (entry.metadata.keywords || []).join(' ');
        const ents = (entry.metadata.entities || []).join(' ');
        this._upsertStmt.run(entry.id, entry.path, entry.summary, entry.text, entry.importance,
            entry.event_time, entry.record_time, entry.category, entry.access_count, entry.last_access, meta);
        this._ftsDelete.run(entry.id);
        this._ftsInsert.run(entry.id, entry.path, entry.summary, entry.text, kws, ents);
    }

    search(query, topK = 6) {
        // FTS search
        const safe = query.replace(/[^\w\s\u4e00-\u9fff]/g, '').trim();
        if (!safe) return [];
        let ftsResults;
        try {
            ftsResults = this._ftsSearch.all(safe, topK * 3);
        } catch { ftsResults = []; }
        if (ftsResults.length === 0) return [];

        // Normalize scores
        const maxScore = Math.max(...ftsResults.map(r => r.score), 1);

        // Fetch full entries
        const ids = ftsResults.map(r => r.id);
        const placeholders = ids.map(() => '?').join(',');
        const entries = this.db.prepare(
            `SELECT * FROM memories WHERE id IN (${placeholders})`
        ).all(...ids);

        const entryMap = {};
        entries.forEach(e => { entryMap[e.id] = e; });

        // Score and rank (simplified hybrid — FTS + symbolic, no vec)
        const results = ftsResults.map(r => {
            const entry = entryMap[r.id];
            if (!entry) return null;
            const ftsScore = r.score / maxScore;

            // Symbolic scoring (tokenize + overlap)
            const qTokens = safe.toLowerCase().split(/[^\w\u4e00-\u9fff]+/).filter(t => t.length >= 2);
            const tTokens = entry.text.toLowerCase().split(/[^\w\u4e00-\u9fff]+/).filter(t => t.length >= 2);
            const overlap = qTokens.filter(t => tTokens.includes(t)).length;
            const symScore = qTokens.length > 0 ? overlap / qTokens.length : 0;

            const finalScore = 0.3 * ftsScore + 0.2 * symScore;
            return { entry, score: { fts: ftsScore, symbolic: symScore, final: finalScore } };
        }).filter(Boolean);

        results.sort((a, b) => b.score.final - a.score.final);
        return results.slice(0, topK);
    }

    get(id) {
        return this._getById.get(id) || null;
    }
}

// ── Benchmark ─────────────────────────────────────────────────────────────────
async function benchOld() {
    console.log('═══════════════════════════════════════════════════════');
    console.log('  better-sqlite3 (Old) Performance Benchmark');
    console.log('═══════════════════════════════════════════════════════\n');

    if (fs.existsSync(DB_PATH)) fs.unlinkSync(DB_PATH);
    const memBefore = process.memoryUsage();
    const store = new OldStore(DB_PATH);

    // 1. Bulk Upsert
    console.log(`▸ Upserting ${NUM_ENTRIES} entries...`);
    const upsertTimes = [];
    const bulkStart = performance.now();
    for (let i = 0; i < NUM_ENTRIES; i++) {
        const entry = generateEntry(i);
        const t0 = performance.now();
        store.upsert(entry);
        upsertTimes.push(performance.now() - t0);
    }
    const bulkDuration = performance.now() - bulkStart;
    const uS = stats(upsertTimes);
    console.log(`  Total: ${bulkDuration.toFixed(1)}ms | ${(NUM_ENTRIES / bulkDuration * 1000).toFixed(0)} ops/sec`);
    console.log(`  Per-op: mean=${uS.mean}ms  p50=${uS.p50}ms  p95=${uS.p95}ms  p99=${uS.p99}ms\n`);

    // 2. Search (EN)
    const queries_en = ['Rust performance', 'Python machine learning', 'Docker deploy', 'Redis cache', 'WebSocket'];
    console.log(`▸ Running ${SEARCH_ITERATIONS} English searches...`);
    const searchTimesEn = [];
    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const t0 = performance.now();
        store.search(queries_en[i % queries_en.length], 6);
        searchTimesEn.push(performance.now() - t0);
    }
    const sEn = stats(searchTimesEn);
    console.log(`  Per-search: mean=${sEn.mean}ms  p50=${sEn.p50}ms  p95=${sEn.p95}ms  p99=${sEn.p99}ms\n`);

    // 3. Search (CN)
    const queries_cn = ['系统编程', '机器学习', '数据库性能', '缓存策略', '容器化部署'];
    console.log(`▸ Running ${SEARCH_ITERATIONS} Chinese searches...`);
    const searchTimesCn = [];
    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const t0 = performance.now();
        store.search(queries_cn[i % queries_cn.length], 6);
        searchTimesCn.push(performance.now() - t0);
    }
    const sCn = stats(searchTimesCn);
    console.log(`  Per-search: mean=${sCn.mean}ms  p50=${sCn.p50}ms  p95=${sCn.p95}ms  p99=${sCn.p99}ms\n`);

    // 4. Get by ID
    console.log(`▸ Running ${SEARCH_ITERATIONS} get-by-ID...`);
    const getTimes = [];
    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const id = `bench-${String(i % NUM_ENTRIES).padStart(5, '0')}`;
        const t0 = performance.now();
        store.get(id);
        getTimes.push(performance.now() - t0);
    }
    const gS = stats(getTimes);
    console.log(`  Per-get: mean=${gS.mean}ms  p50=${gS.p50}ms  p95=${gS.p95}ms  p99=${gS.p99}ms\n`);

    // 5. Memory
    const memAfter = process.memoryUsage();
    const heapDelta = ((memAfter.heapUsed - memBefore.heapUsed) / 1024 / 1024).toFixed(2);
    const rssDelta = ((memAfter.rss - memBefore.rss) / 1024 / 1024).toFixed(2);
    console.log(`▸ Memory: heap Δ${heapDelta}MB  |  RSS Δ${rssDelta}MB`);
    const dbSize = fs.statSync(DB_PATH).size;
    console.log(`▸ DB file size: ${(dbSize / 1024).toFixed(1)}KB\n`);

    console.log(`  Operation       │ Mean(ms) │ P50(ms) │ P95(ms) │ P99(ms)`);
    console.log(`  ────────────────┼──────────┼─────────┼─────────┼────────`);
    console.log(`  Upsert          │ ${uS.mean.padStart(8)} │ ${uS.p50.padStart(7)} │ ${uS.p95.padStart(7)} │ ${uS.p99.padStart(7)}`);
    console.log(`  Search (EN)     │ ${sEn.mean.padStart(8)} │ ${sEn.p50.padStart(7)} │ ${sEn.p95.padStart(7)} │ ${sEn.p99.padStart(7)}`);
    console.log(`  Search (CN)     │ ${sCn.mean.padStart(8)} │ ${sCn.p50.padStart(7)} │ ${sCn.p95.padStart(7)} │ ${sCn.p99.padStart(7)}`);
    console.log(`  Get by ID       │ ${gS.mean.padStart(8)} │ ${gS.p50.padStart(7)} │ ${gS.p95.padStart(7)} │ ${gS.p99.padStart(7)}`);

    fs.unlinkSync(DB_PATH);
}

benchOld();
