// Quick smoke test for the memory-core NAPI binding
const { JsMemoryStore } = require('./index.js');
const fs = require('fs');
const path = require('path');

const TEST_DB = path.join(__dirname, '_test_napi.db');

// Clean up from previous runs
if (fs.existsSync(TEST_DB)) fs.unlinkSync(TEST_DB);

try {
    // 1) Open store
    const store = new JsMemoryStore(TEST_DB);
    console.log('✅ Store opened, vec_available:', store.vecAvailable);

    // 2) Upsert an entry
    const entry = {
        id: 'test-001',
        path: '/test',
        summary: 'Rust is fast',
        text: 'Rust is a systems programming language focused on safety and performance',
        importance: 0.8,
        event_time: new Date().toISOString(),
        record_time: new Date().toISOString(),
        category: 'fact',
        access_count: 0,
        last_access: null,
        metadata: { keywords: ['rust', 'performance', 'safety'], entities: [] },
    };
    store.upsert(JSON.stringify(entry));
    console.log('✅ Upsert succeeded');

    // 3) Upsert a Chinese entry
    const cnEntry = {
        id: 'test-002',
        path: '/test',
        summary: '记忆系统',
        text: '记忆系统使用混合检索来提高搜索质量',
        importance: 0.7,
        event_time: new Date().toISOString(),
        record_time: new Date().toISOString(),
        category: 'fact',
        access_count: 0,
        last_access: null,
        metadata: { keywords: ['记忆', '检索', '混合'], entities: [] },
    };
    store.upsert(JSON.stringify(cnEntry));
    console.log('✅ Chinese entry upserted');

    // 4) Search (English)
    const results = JSON.parse(store.search('rust performance'));
    console.log(`✅ Search "rust performance": ${results.length} result(s)`);
    if (results.length > 0) {
        console.log('   → top hit:', results[0].entry.id, '| score:', results[0].score.final.toFixed(4));
    }

    // 5) Search (Chinese)
    const cnResults = JSON.parse(store.search('记忆检索'));
    console.log(`✅ Search "记忆检索": ${cnResults.length} result(s)`);
    if (cnResults.length > 0) {
        console.log('   → top hit:', cnResults[0].entry.id, '| score:', cnResults[0].score.final.toFixed(4));
    }

    // 6) Get by ID
    const e1 = JSON.parse(store.get('test-001'));
    console.log(`✅ Get by ID: test-001 | text length: ${e1.text.length}`);

    // Test get_all
    const allEntries = JSON.parse(store.getAll());
    console.log(`✅ Get All entries count: ${allEntries.length}`);
    if (allEntries.length !== 2) {
        throw new Error('getAll should return exactly 2 entries');
    }

    console.log('\n🎉 All smoke tests passed!');
} catch (err) {
    console.error('❌ Test failed:', err.message);
    process.exit(1);
} finally {
    // Clean up
    if (fs.existsSync(TEST_DB)) fs.unlinkSync(TEST_DB);
}
