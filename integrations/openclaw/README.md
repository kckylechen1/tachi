# memory-hybrid-bridge (OpenClaw Plugin)

OpenClaw 记忆插件 — 通过 Tachi MCP server 提供混合检索（向量 + FTS + 符号 + 衰减），支持 LLM 结构化提取、去重合并、自动上下文注入。

## 架构

```
OpenClaw Gateway (Node.js)
  └─ memory-hybrid-bridge plugin (this package)
       ├─ MCP client ──→ tachi / memory-server (Rust binary, stdio transport)
       │     └─ SQLite + sqlite-vec (memory.db)
       └─ NAPI fallback (optional @chaoxlabs/tachi-node native binding)
```

**MCP 优先**：默认通过 MCP stdio 协议调用 Tachi 二进制。当 MCP 不可用时自动降级到 NAPI（30s 重试窗口）。
环境变量 `OPENCLAW_MEMORY_BACKEND=napi` 可强制使用 NAPI 路径。

## 安装

### 方式一：brew install（推荐）

```bash
brew tap kckylechen1/tachi && brew install tachi
```

插件会自动在 PATH 中查找 `tachi` 二进制。

### 方式二：源码构建

```bash
curl -fsSL https://raw.githubusercontent.com/kckylechen1/tachi/main/scripts/install_openclaw_ext.sh | bash
```

需要 `git` + `node` + `npm`。如果安装了 `cargo`，会额外编译 NAPI 原生模块；否则以 MCP-only 模式运行。

## 关键文件

| 文件 | 职责 |
|------|------|
| `index.ts` | 插件入口：tools、hooks、agent-scoped stores、审计日志 |
| `store.ts` | `MemoryStore` — MCP→NAPI 双后端 + `withBackend()` fallback |
| `mcp-client.ts` | MCP stdio client — 多候选启动、连接恢复、JSON 解析 |
| `extractor.ts` | LLM 结构化提取 + 输入清洗 + category-aware merge |
| `config.ts` | 类型定义 + 默认配置（从环境变量读取） |
| `constants.ts` | 环境加载：`~/.secrets/master.env` → `~/.tachi/config.env` |
| `reranker.ts` | Voyage rerank-2.5 重排序 |

## 环境变量

秘钥通过以下链加载（`constants.ts`）：
1. `~/.secrets/master.env` — 基础秘钥（backfill，不覆盖已有）
2. `~/.tachi/config.env` — 覆盖（override=true）
3. `process.env`（direnv/.envrc）优先级最高

| 变量 | 必填 | 说明 |
|------|------|------|
| `VOYAGE_API_KEY` | 是 | Voyage AI embedding + reranking |
| `SILICONFLOW_API_KEY` | 是 | SiliconFlow LLM 提取 |
| `MEMORY_DB_PATH` | 否 | 数据库路径（默认 `~/.tachi/memory.db`） |
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

## 注册的 Hooks

| Hook | 说明 |
|------|------|
| `before_agent_start` | FTS-only 零延迟检索，注入 `<relevant-structured-memories>` 上下文 |
| `agent_end` | 自动捕获：LLM 提取 → 嵌入 → 去重/合并 → 写入 |

## 回滚

1. 在 `openclaw.json` 中禁用 `memory-hybrid-bridge`
2. 如需清理数据：删除 `~/.tachi/memory.db`
3. 重启 OpenClaw gateway
