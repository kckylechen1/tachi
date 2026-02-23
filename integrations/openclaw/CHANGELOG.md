# CHANGELOG

## 0.2.0 - 2026-02-12

### P0 安全与稳健

- extractor 前新增输入清洗：控制字符、零宽字符、常见注入短语与伪角色标签中和。
- prompt 加载新增内置 fallback，外部模板缺失时自动降级可用。
- memory 关键写操作新增审计日志 `data/audit-log.jsonl`：append / overwrite / delete。

### P1 质量与性能

- 新增提炼去重机制（默认阈值 `0.9`），避免 shadow store 膨胀。
- 新增自动捕获触发器（关键词 + 最小长度），减少无意义提炼调用。
- hybrid search 新增“最近 N 条限量读取”（默认 2000）与短 TTL 缓存，降低全量扫描开销。

### 工具与配置

- 新增工具：`memory_delete_entry`（删除 entry 并审计）。
- 配置新增：`auditLogPath`、`searchReadLimit`、`dedupThreshold`、`captureMinChars`、`captureTriggerKeywords`。

### 文档

- README 增补“为什么这些改造”与最小验证命令。
