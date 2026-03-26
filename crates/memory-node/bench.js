/**
 * Performance Benchmark: Rust NAPI vs better-sqlite3
 *
 * Tests: upsert latency, search latency, bulk operations, memory usage
 * Runs both backends on the same dataset for fair comparison.
 */

const { JsMemoryStore } = require('./index.js');
const path = require('path');
const fs = require('fs');

// ── Config ────────────────────────────────────────────────────────────────────
const NUM_ENTRIES = 1000;
const SEARCH_ITERATIONS = 200;
const DB_PATH = path.join(__dirname, '_bench.db');

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
            keywords: [
                topics[i % topics.length].slice(0, 4),
                `kw-${i % 20}`,
                'benchmark'
            ],
            entities: [`entity-${i % 15}`],
        },
    };
}

function percentile(arr, p) {
    const sorted = [...arr].sort((a, b) => a - b);
    const idx = Math.ceil(sorted.length * p / 100) - 1;
    return sorted[Math.max(0, idx)];
}

function stats(arr) {
    const sum = arr.reduce((a, b) => a + b, 0);
    return {
        mean: (sum / arr.length).toFixed(3),
        p50: percentile(arr, 50).toFixed(3),
        p95: percentile(arr, 95).toFixed(3),
        p99: percentile(arr, 99).toFixed(3),
        min: Math.min(...arr).toFixed(3),
        max: Math.max(...arr).toFixed(3),
    };
}

// ── Benchmark ─────────────────────────────────────────────────────────────────
async function benchRustNAPI() {
    console.log('═══════════════════════════════════════════════════════');
    console.log('  Rust NAPI Binding Performance Benchmark');
    console.log('═══════════════════════════════════════════════════════\n');

    // Cleanup
    if (fs.existsSync(DB_PATH)) fs.unlinkSync(DB_PATH);

    const memBefore = process.memoryUsage();
    const store = new JsMemoryStore(DB_PATH);

    // ── 1. Bulk Upsert ────────────────────────────────────────────────────────
    console.log(`▸ Upserting ${NUM_ENTRIES} entries...`);
    const upsertTimes = [];
    const bulkStart = performance.now();

    for (let i = 0; i < NUM_ENTRIES; i++) {
        const entry = generateEntry(i);
        const t0 = performance.now();
        store.upsert(JSON.stringify(entry));
        upsertTimes.push(performance.now() - t0);
    }

    const bulkDuration = performance.now() - bulkStart;
    const upsertStats = stats(upsertTimes);
    console.log(`  Total: ${bulkDuration.toFixed(1)}ms | ${(NUM_ENTRIES / bulkDuration * 1000).toFixed(0)} ops/sec`);
    console.log(`  Per-op: mean=${upsertStats.mean}ms  p50=${upsertStats.p50}ms  p95=${upsertStats.p95}ms  p99=${upsertStats.p99}ms\n`);

    // ── 2. Search Latency (English) ───────────────────────────────────────────
    const queries_en = ['Rust performance', 'Python machine learning', 'Docker deploy', 'Redis cache', 'WebSocket'];
    console.log(`▸ Running ${SEARCH_ITERATIONS} English searches...`);
    const searchTimesEn = [];

    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const q = queries_en[i % queries_en.length];
        const t0 = performance.now();
        const results = JSON.parse(store.search(q, 6));
        searchTimesEn.push(performance.now() - t0);
    }

    const searchStatsEn = stats(searchTimesEn);
    console.log(`  Per-search: mean=${searchStatsEn.mean}ms  p50=${searchStatsEn.p50}ms  p95=${searchStatsEn.p95}ms  p99=${searchStatsEn.p99}ms\n`);

    // ── 3. Search Latency (Chinese) ───────────────────────────────────────────
    const queries_cn = ['系统编程', '机器学习', '数据库性能', '缓存策略', '容器化部署'];
    console.log(`▸ Running ${SEARCH_ITERATIONS} Chinese searches...`);
    const searchTimesCn = [];

    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const q = queries_cn[i % queries_cn.length];
        const t0 = performance.now();
        const results = JSON.parse(store.search(q, 6));
        searchTimesCn.push(performance.now() - t0);
    }

    const searchStatsCn = stats(searchTimesCn);
    console.log(`  Per-search: mean=${searchStatsCn.mean}ms  p50=${searchStatsCn.p50}ms  p95=${searchStatsCn.p95}ms  p99=${searchStatsCn.p99}ms\n`);

    // ── 4. Get by ID ──────────────────────────────────────────────────────────
    console.log(`▸ Running ${SEARCH_ITERATIONS} get-by-ID...`);
    const getTimes = [];
    for (let i = 0; i < SEARCH_ITERATIONS; i++) {
        const id = `bench-${String(i % NUM_ENTRIES).padStart(5, '0')}`;
        const t0 = performance.now();
        store.get(id);
        getTimes.push(performance.now() - t0);
    }
    const getStats = stats(getTimes);
    console.log(`  Per-get: mean=${getStats.mean}ms  p50=${getStats.p50}ms  p95=${getStats.p95}ms  p99=${getStats.p99}ms\n`);

    // ── 5. Memory ─────────────────────────────────────────────────────────────
    const memAfter = process.memoryUsage();
    const heapDelta = ((memAfter.heapUsed - memBefore.heapUsed) / 1024 / 1024).toFixed(2);
    const rssDelta = ((memAfter.rss - memBefore.rss) / 1024 / 1024).toFixed(2);
    console.log(`▸ Memory: heap Δ${heapDelta}MB  |  RSS Δ${rssDelta}MB`);

    // ── 6. DB size ────────────────────────────────────────────────────────────
    const dbSize = fs.statSync(DB_PATH).size;
    console.log(`▸ DB file size: ${(dbSize / 1024).toFixed(1)}KB\n`);

    // Cleanup
    fs.unlinkSync(DB_PATH);

    return {
        upsert: upsertStats,
        searchEn: searchStatsEn,
        searchCn: searchStatsCn,
        get: getStats,
        heapDelta,
        rssDelta,
        dbSizeKB: (dbSize / 1024).toFixed(1),
    };
}

// ── Run ───────────────────────────────────────────────────────────────────────
benchRustNAPI().then(results => {
    console.log('═══════════════════════════════════════════════════════');
    console.log('  Summary Table');
    console.log('═══════════════════════════════════════════════════════');
    console.log('');
    console.log(`  Operation       │ Mean(ms) │ P50(ms) │ P95(ms) │ P99(ms)`);
    console.log(`  ────────────────┼──────────┼─────────┼─────────┼────────`);
    console.log(`  Upsert          │ ${results.upsert.mean.padStart(8)} │ ${results.upsert.p50.padStart(7)} │ ${results.upsert.p95.padStart(7)} │ ${results.upsert.p99.padStart(7)}`);
    console.log(`  Search (EN)     │ ${results.searchEn.mean.padStart(8)} │ ${results.searchEn.p50.padStart(7)} │ ${results.searchEn.p95.padStart(7)} │ ${results.searchEn.p99.padStart(7)}`);
    console.log(`  Search (CN)     │ ${results.searchCn.mean.padStart(8)} │ ${results.searchCn.p50.padStart(7)} │ ${results.searchCn.p95.padStart(7)} │ ${results.searchCn.p99.padStart(7)}`);
    console.log(`  Get by ID       │ ${results.get.mean.padStart(8)} │ ${results.get.p50.padStart(7)} │ ${results.get.p95.padStart(7)} │ ${results.get.p99.padStart(7)}`);
    console.log('');
    console.log(`  Heap: Δ${results.heapDelta}MB  │  RSS: Δ${results.rssDelta}MB  │  DB: ${results.dbSizeKB}KB`);
    console.log('');
});
