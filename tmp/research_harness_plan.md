# Research Harness Plan: Memory Hybrid Bridge Stability

## 1. 根因分析 (Root Cause Analysis)

### P0: 异步调用链“闭环差”与“写不完”
1.  **Event Emit & Forget**: `index.ts` 中的 `agent_end` 钩子虽然是 `async`，但 OpenClaw 插件系统的 `api.on('agent_end', ...)` 通常在核心逻辑结束后并行触发，且不保证阻塞主进程退出。如果主进程直接退出，由于 Node.js 事件循环清空，未完成的 LLM 提炼（`extractMemoryEntry`）和数据库写入会被强制中止。
2.  **串行阻塞依赖**: `extractMemoryEntry` -> `getEmbedding` -> `store.upsert` 是严格串行的。一旦 `fetch` 网络波动，整个流程会卡在第一步，且由于缺乏有效的持久化队列，这些内存中待处理的任务会随进程结束而丢失。

### P1: 超时 (Timeout) 处理不当
1.  **硬超时设定**: `config.ts` 默认 `timeoutMs` 为 25000ms。对于复杂的 LLM 提炼（尤其是使用低配或远程 API 时），这个时间在长对话窗口下可能不足。
2.  **Lack of Error Resilience**: 一旦 `AbortController` 触发或 API 返回非 200，代码仅通过 `logger.warn` 打印日志即返回 `null`，没有任何重试机制。

### P2: 资源初始化竞争
1.  **Lazy Initialization**: `ensureStore` 在首次 `performSearch` 或 `agent_end` 时才初始化。如果多个 Agent 同时触发，`initStores` 的 Map 检查虽然用了 `Promise`，但在高并发下可能导致 SQLite 连接池死锁或文件系统权限冲突。

---

## 2. 可执行修复方案 (Actionable Fixes)

### 2.1 命令模板 (Validation & Fix)

**诊断环境脚本 (`diag_memory.sh`):**
```bash
#!/bin/bash
# 验证关键路径与环境变量
WORKSPACE="/Users/kckylechen/Desktop/Sigil"
PROJECT_DIR="$WORKSPACE/integrations/openclaw"
DB_PATH="$WORKSPACE/extensions/memory-hybrid-bridge/data/memory.db"

echo "Checking environment..."
[ -f "$PROJECT_DIR/index.ts" ] && echo "OK: Source exists" || { echo "ERR: Source missing"; exit 1; }
[ -d "$(dirname "$DB_PATH")" ] || mkdir -p "$(dirname "$DB_PATH")"

# 检查环境变量
echo "API Key Status:"
[ -z "$SILICONFLOW_API_KEY" ] && echo "WARN: SILICONFLOW_API_KEY is empty"
[ -z "$VOYAGE_API_KEY" ] && echo "WARN: VOYAGE_API_KEY is empty"

exit 0
```

### 2.2 超时与重试策略 (Timeout & Retry Strategy)

**1. 引入指数退避重试 (Exponential Backoff):**
在 `extractor.ts` 中封装 `fetchWithRetry`。
- **Max Retries**: 3
- **Initial Delay**: 1000ms
- **Factor**: 2
- **Retryable Errors**: 429 (Rate Limit), 500/502/503/504, `ECONNRESET`, `ETIMEDOUT`.

**2. 梯度超时:**
- `Extractor`: 45s (复杂推理需要更多时间)
- `Embedding`: 10s (通常很快)

### 2.3 流程优化 (Closing the Loop)

**关键点：将 `agent_end` 转化为“可靠交付”模式。**

1.  **持久化暂存 (Side-car Persistence)**:
    - 在 LLM 提炼前，将 `inputWindowText` 写入临时 JSONL 文件（`tmp/pending_memories.jsonl`）。
    - 提炼成功后，从文件中删除该条目。
    - 插件启动时，检查并重跑暂存的任务。

2.  **手动 Flush**:
    - 在 `OpenClawPluginApi` 中如果支持，应显式声明 `agent_end` 需要等待该 Promise 完成（`await api.emit(...)`）。

---

## 3. 失败分级规则 (Failure Classification)

| 等级 | 现象 | 处理方案 |
| :--- | :--- | :--- |
| **P0 (Critical)** | SQLite 文件损坏 / 权限拒绝 | 立即停止服务，备份并尝试重建索引 (Re-indexing from Audit Log) |
| **P1 (Major)** | API 持续 5xx / 认证失败 | 暂停 Capture 队列，发送 System Notification 给用户，30分钟后自动检测恢复 |
| **P2 (Minor)** | 单条记忆 JSON 解析失败 | 丢弃该条目，但在 Audit Log 中记录原始文本以便人工调试 |
| **P2 (Minor)** | Embedding 结果为空 | 降级为仅 Lexical (FTS) 搜索，不阻塞流程 |

---

## 4. 后续任务摘要

1.  **实现 `BackoffFetch`**: 替代 `extractor.ts` 中的原生 `fetch`。
2.  **增加 `RecoveryService`**: 在插件 `register` 时启动一个扫描器处理 `tmp/pending_memories.jsonl`。
3.  **优化提示词**: 在 `FALLBACK_PROMPT` 中增加 `Strict Mode` 要求，减少 JSON 截断概率。

**报告生成路径**: `/Users/kckylechen/Desktop/Sigil/tmp/research_harness_plan.md`
