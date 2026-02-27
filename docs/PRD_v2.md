# Sigil v2: 自治型自进化记忆智能体 PRD

**文档版本**: v2.1 (生产就绪版)
**修订**: 吸收 Codex & Perplexity 审查意见
**日期**: 2026-02-27

---

## 1. 核心系统架构 (Event-Driven Architecture)

Sigil v2 采用**事件溯源 (Event Sourcing)** 架构。客户端仅发送对话事件，所有记忆处理全异步化。

### 1.1 队列表 schema (memory_events)
修复前序版本中“状态记录不清”的问题：
```sql
CREATE TABLE memory_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL,         -- 业务幂等 ID: hash(conv_id + turn_id)
    worker_type TEXT NOT NULL,      -- 'extractor', 'causal', 'consolidator', 'distiller'
    status TEXT DEFAULT 'PENDING',  -- 'PENDING', 'PROCESSING', 'DONE', 'FAILED'
    payload JSON NOT NULL,
    retry_count INT DEFAULT 0,
    locked_until DATETIME,          -- 租约锁，防止多实例重刷
    last_error TEXT,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    processed_at DATETIME
);
```

### 1.2 记忆项 schema (memories)
引入并发控制与物理隔离：
```sql
CREATE TABLE memories (
    id TEXT PRIMARY KEY,            -- UUID
    text TEXT NOT NULL,
    embedding BLOB,                 -- vector
    metadata JSON,                  -- {path, topic, importance, origin, ...}
    revision INTEGER DEFAULT 1,     -- 乐观锁，用于 Consolidator 合并
    archived BOOLEAN DEFAULT FALSE, -- 软删除标志
    created_at DATETIME,
    updated_at DATETIME
);
```

---

## 2. Worker 行为细则与防护措施

### 2.1 Worker 1 & 3: 并行提取 (Extractor & Causal Analyzer)
- **并发策略**: 按 `conversation_id` 串行处理（或在保存事实时使用 `INSERT OR IGNORE` 结合业务幂等键）。
- **幂等保证**: ID 生成公式为 `hash(event_id + chunk_index)`。

### 2.2 Worker 2: Consolidator (语义合并器) - **高风险区**
- **竞态防护**: 
  - 必须使用 **乐观锁 (Revision)**。写回合并后的记忆时，校验目标记忆的 `revision` 是否改变。
  - 认领任务时使用 `BEGIN IMMEDIATE` 进行原子标记。
- **防止循环**: 只有 `origin=extraction` 的新记忆会触发合并；`origin=consolidation` 的写回操作不再触发自身。

### 2.3 Worker 4: Distiller (规则蒸馏器) - **抗幻觉增强**
- **触发阈值**: 仅当特定路径下 `origin=causal` 的**独立纠正片段数量 >= 5** 时触发。
- **Prompt 约束**:
  > "如果样本不足或无法提取出跨场景的通用性，必须返回 `[]`，严禁强行总结具体事件。"
- **规则生命周期**: `DRAFT` (初次生成) → `ACTIVE` (审核/自验后) → `SUPERSEDED` (被新版本覆盖) → `DISABLED` (手动弃用)。

---

## 3. 检索路径下推 (Retrieval Planner)

- **性能优化**: 彻底废弃 Python 层的 `get_all(5000)` 大规模扫描。
- **SQL 实现**:
  ```sql
  SELECT * FROM memories 
  WHERE archived = FALSE 
    AND JSON_EXTRACT(metadata, '$.path') LIKE :path_prefix
    AND (
      -- Hybrid Scoring: Vector + FTS
      ...
    )
  LIMIT 20;
  ```
- **L0/L1/L2 加载**: 优先加载规则路径 (`/behavior/global_rules/*`) 作为 L0 强制上下文。

---

## 4. 安全与数据治理 (Security & Governance)

- **PII 治理**: Extractor 在提取事实时，自动对敏感信息（如密码、身份证、私密地址）进行脱敏处理。
- **认证鉴权**: API 访问需携带 `X-Sigil-Key`。
- **时区强制**: 数据库级强制存储 UTC，仅在 API 展示层根据客户端 Locale 转换展示。

---

## 5. 迁移与验证 (Shadow Release)

### 5.1 数据一致性核对 (Check-sum)
在 Phase 3 (双写阶段)，每天凌晨 3:00 运行 `consistency_check.py`：
- 对比老 Bridge 与新 API 在同时间段内的事件接收总数。
- 若 `abs(count_old - count_new) > 0`，则触发钉钉/Slack 告警。

### 5.2 割接门禁 (Cutover Gates)
只有满足以下条件才能断开老桥接：
1. **成功率**: Worker 任务失败率 < 0.1% (持续 48 小时)。
2. **检索延迟**: P95 < 200ms。
3. **记忆密度**: 通过随机抽样，验证 Consolidator 的去重率在 30%-50% 之间（且无关键事实丢失）。

---

## 6. 工具与代码重构 (Must-fix)

1. **db.rs**: 
   - 移除所有硬编码的 `+08:00`。
   - 实现 `begin_transaction()` 宏/装饰器。
2. **store.py**:
   - 升级 `list_by_path` 支持真正的 SQL 级别过滤。
3. **server.py**:
   - 增加请求 Body 的 JSON Schema 验证。

---
> "The strength of a memory agent lies in the rigor of its governance."
