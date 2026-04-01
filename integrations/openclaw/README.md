# tachi (OpenClaw Plugin)

OpenClaw 统一记忆插件 — 通过 Tachi MCP server 提供混合检索（向量 + FTS + 符号 + 衰减），并合并上下文提纯、任务跟踪、运行审计与 Tachi 子系统直通能力。

## 架构

```
OpenClaw Gateway (Node.js)
  └─ tachi plugin (this package)
       ├─ MCP client ──→ tachi / memory-server (Rust binary, stdio transport)
       │     └─ SQLite + sqlite-vec (memory.db)
       └─ NAPI fallback (optional @chaoxlabs/tachi-node native binding)
```

**MCP 优先**：默认通过 MCP stdio 协议调用 Tachi 二进制。当 MCP 不可用时自动降级到 NAPI（30s 重试窗口）。
环境变量 `OPENCLAW_MEMORY_BACKEND=napi` 可强制使用 NAPI 路径。

**当前运行时拓扑**：插件通过 `getResolvedPaths(agentId)` 将记忆按 agent 拆到 `data/agents/<agent>/memory.db`。根目录的 `data/memory.db` 只作为历史/迁移遗留库保留，不再是新写入的默认目标。

## 安装

### 一键安装（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/kckylechen1/tachi/main/scripts/install.sh | bash
```

该脚本会：
- 通过 Homebrew 安装或升级 `tachi`
- 下载并安装 OpenClaw `tachi` 插件
- 自动更新 `~/.openclaw/openclaw.json` 中的 `plugins.allow`、`plugins.load.paths` 与 `plugins.slots.memory`

### 仅安装 OpenClaw 插件

```bash
curl -fsSL https://raw.githubusercontent.com/kckylechen1/tachi/main/scripts/install_openclaw_ext.sh | bash
```

这是兼容旧流程的包装脚本，等价于执行 `scripts/install.sh --skip-brew`。

## 关键文件

| 文件 | 职责 |
|------|------|
| `index.ts` | 插件入口：tools、hooks、agent-scoped stores、审计日志 |
| `store.ts` | `MemoryStore` — MCP→NAPI 双后端 + `withBackend()` fallback |
| `mcp-client.ts` | MCP stdio client — 多候选启动、连接恢复、JSON 解析 |
| `extractor.ts` | LLM 结构化提取 + 输入清洗 + category-aware merge |
| `config.ts` | 类型定义 + 默认配置（从环境变量读取） |
| `constants.ts` | 环境加载：`.env` + 运行时环境变量 |
| `reranker.ts` | Voyage rerank-2.5 重排序 |

## 环境变量

将 `.env.example` 拷贝为 `.env`（项目根目录或插件目录均可），填入 API 密钥。
插件运行时会自动从 `.env` 加载环境变量。

| 变量 | 必填 | 说明 |
|------|------|------|
| `VOYAGE_API_KEY` | 是 | Voyage AI embedding + reranking |
| `SILICONFLOW_API_KEY` | 是 | SiliconFlow LLM 提取 |
| `TACHI_BIN` / `OPENCLAW_MEMORY_SERVER_BIN` | 否 | 显式指定 `tachi` / `memory-server` 二进制路径，优先于 PATH |
| `OPENCLAW_MEMORY_BACKEND` | 否 | `mcp`（默认）或 `napi` |
| `MEMORY_BRIDGE_EMBEDDING_MODEL` | 否 | 嵌入模型（默认 `voyage-4`） |
| `MEMORY_BRIDGE_EMBEDDING_DIMENSION` | 否 | 嵌入维度（默认 1024） |
| `MEMORY_BRIDGE_DEDUP_THRESHOLD` | 否 | 去重阈值（默认 0.95） |
| `MEMORY_BRIDGE_MERGE_THRESHOLD` | 否 | 合并阈值（默认 0.85） |

完整列表见 [`.env.example`](./.env.example)。

## 注册的 Tools

| Tool 名称 | 说明 |
|-----------|------|
| `memory_search` | 语义混合检索（向量 + FTS + rerank） |
| `memory_hybrid_search` | 同上（兼容别名） |
| `memory_get` | 按 ID 获取单条记忆 |
| `compact_context` | 提纯当前会话，并将 compact 摘要注入 system event |
| `todo_write` / `todo_read` / `spawn_tasks` | 会话 todo 与子代理 spawn 跟踪 |
| `tachi_*` | 直通 skill / hub / vault / ghost / kanban / graph / state / identity / handoff |

## 注册的 Hooks

| Hook | 说明 |
|------|------|
| `before_agent_start` | FTS-only 零延迟检索，注入 `<relevant-structured-memories>` 上下文 |
| `agent_end` | 自动捕获：LLM 提取 → 嵌入 → 去重/合并 → 写入 |
| `llm_input` / `llm_output` / `before_compaction` / `after_compaction` | usage + compaction 跟踪 |
| `after_tool_call` | tooluse 样本、spawn 跟踪、sessions_spawn 审计 |
| `session_end` / `tool_result_persist` / `subagent_spawned` / `subagent_ended` | snapshot、compact 注入、子代理生命周期审计 |

## 回滚

1. 在 `openclaw.json` 中禁用 `tachi`
2. 如需清理数据：删除 `data/agents/<agent>/memory.db` 或整个插件 `data/agents/` 目录
3. 重启 OpenClaw gateway
