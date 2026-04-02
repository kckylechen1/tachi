# tachi (OpenClaw Plugin)

OpenClaw 统一记忆插件 — 作为 Tachi kernel 的轻量 runtime facade，负责 agent-facing 的记忆工具面、生命周期 hooks，以及上下文注入。

## 架构

```
OpenClaw Gateway (Node.js)
  └─ tachi plugin (this package)
       └─ MCP client ──→ tachi / memory-server (Rust binary, stdio transport)
             └─ SQLite + sqlite-vec (memory.db)
```

**MCP-only**：插件默认通过 MCP stdio 协议调用 Tachi 二进制，记忆提炼、embedding、rerank、distill、graph maintenance 都在 Tachi 侧完成。

**当前运行时拓扑**：OpenClaw 插件不再维护本地 shadow store、SQLite FTS 或 capture spool；它只负责 hook timing、tool exposure 和调用 Tachi runtime APIs。

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
| `index.ts` | 插件入口：仅暴露 `memory_search / memory_save / memory_get / memory_graph`，并在 hooks 中调用 Tachi runtime APIs |
| `mcp-client.ts` | MCP stdio client — 多候选启动、连接恢复、JSON 解析 |
| `config.ts` | 类型定义 + 默认配置（从环境变量读取） |
| `constants.ts` | 环境加载：`.env` + 运行时环境变量 |

## 环境变量

将 `.env.example` 拷贝为 `.env`（项目根目录或插件目录均可），填入运行所需变量。
插件运行时会自动从 `.env` 加载环境变量。

| 变量 | 必填 | 说明 |
|------|------|------|
| `TACHI_BIN` / `OPENCLAW_MEMORY_SERVER_BIN` | 否 | 显式指定 `tachi` / `memory-server` 二进制路径，优先于 PATH |
| `MEMORY_BRIDGE_CAPTURE_MIN_CHARS` | 否 | 自动捕获最小字符数阈值 |
| `MEMORY_BRIDGE_CAPTURE_TRIGGERS` | 否 | 自动捕获关键词列表 |

完整列表见 [`.env.example`](./.env.example)。

记忆提炼、embedding、rerank 和 distill 所需的模型密钥现在应配置在 Tachi 侧，而不是 OpenClaw 插件侧。

## 注册的 Tools

| Tool 名称 | 说明 |
|-----------|------|
| `memory_search` | 语义混合检索（向量 + FTS + rerank） |
| `memory_save` | 显式写入 durable memory |
| `memory_get` | 按 ID 获取单条记忆 |
| `memory_graph` | 只读查看记忆图谱邻域 |

## 注册的 Hooks

| Hook | 说明 |
|------|------|
| `before_agent_start` | 调用 `recall_context`，注入 `<relevant-structured-memories>` 上下文 |
| `agent_end` | 自动捕获：会话窗口提交到 Tachi，后续维护由 Foundry worker 异步处理 |

`compact_context`、`section.build`、`compact.rollup`、`compact.session_memory` 已经在 Tachi 侧可用；待 OpenClaw SDK 暴露 `before_compaction` 后再接入运行时 hook。

## 回滚

1. 在 `openclaw.json` 中禁用 `tachi`
2. 如需清理数据：删除 `data/agents/<agent>/memory.db` 或整个插件 `data/agents/` 目录
3. 重启 OpenClaw gateway
