# Tachi Hub Phase 2: MCP Client Proxy (hub_invoke)

**Date:** 2026-03-23
**Status:** Design approved, pending implementation

## Problem

Tachi is an MCP server with 15 tools (memory + hub). Agents can discover and retrieve skill content, but cannot call tools from other MCP servers through Tachi. This means:

1. Each agent must independently configure every MCP server it needs
2. There's no unified tool discovery across MCP servers
3. OpenClaw and other frameworks that recently added MCP support still need per-server setup

## Solution

Make Tachi also an **MCP client**. It connects to child MCP servers (GitHub, Slack, Linear, etc.) and exposes their tools as if they were Tachi's own. One config, all tools.

```
Agent → Tachi (MCP server + client) → Child MCP servers
```

## Architecture

```
Agent (Claude/Cursor/Gemini/OpenClaw)
  │ MCP
  ▼
┌────────────────────────────────────────────────┐
│ Tachi Server                                    │
│                                                 │
│  Native Tools (15)        Proxy Tools (dynamic) │
│  save_memory              github__create_pr     │
│  search_memory            github__list_issues   │
│  hub_discover             slack__send_message   │
│  hub_call (fallback)      linear__search_docs   │
│  ...                      ...                   │
│                                                 │
│  ┌───────────────────────────────────────────┐  │
│  │ McpClientPool                              │  │
│  │                                            │  │
│  │  github → ChildConnection (lazy, stdio)    │  │
│  │  slack  → ChildConnection (lazy, stdio)    │  │
│  │  linear → ChildConnection (lazy, SSE)      │  │
│  └───────────────────────────────────────────┘  │
│                                                 │
│  ┌──────────┐  ┌───────────┐                   │
│  │ Global DB│  │ Project DB│                   │
│  └──────────┘  └───────────┘                   │
└────────────────────────────────────────────────┘
  │           │            │
  ▼           ▼            ▼
GitHub MCP  Slack MCP   Linear MCP
(stdio)     (stdio)     (SSE)
```

## Key Decisions

### 1. Tool naming: always prefixed

All proxy tools use `{server_id}__{tool_name}` format. Never unprefixed.

**Rationale:** Unprefixed names become unstable when new servers are added. A tool called `create_issue` today would need renaming when a second server also has `create_issue`. Stable canonical names prevent agent confusion.

Examples: `github__create_pr`, `slack__send_message`, `linear__search_docs`

### 2. Schema discovery: at registration time

When `hub_register(type=mcp)` is called, Tachi:
1. Spawns the child MCP server process
2. Connects as MCP client
3. Calls `list_tools` to discover all available tools
4. Caches the tool schemas in `hub_capabilities.definition`
5. Disconnects

On startup, Tachi reads cached schemas from Hub DB to build the tool list without connecting.

**Rationale:** Agents need tools in `list_tools` before they can call them. Lazy discovery (on first call) doesn't work because the tool wouldn't be in the list at planning time.

### 3. Connection lifecycle: lazy connect + pooled reuse

- **Lazy:** Don't connect on startup. Connect on first actual tool call.
- **Pooled:** Once connected, keep alive for reuse.
- **Idle cleanup:** Disconnect after 5 minutes of inactivity.
- **Health tracking:** Mark unhealthy on crash/timeout, reconnect with backoff.
- **Concurrency:** Default `max_concurrency=1` for stdio servers (conservative). Per-child semaphore.

State machine per child:
```
Disconnected → Connecting → Ready ⟷ Idle(ttl) → Disconnected
                              ↓
                          Unhealthy(backoff)
```

### 4. `hub_call` as stable fallback

Besides the dynamic proxy tools, a stable `hub_call(server_id, tool_name, arguments)` tool is always available. This works even when:
- Cached schema is stale
- Dynamic tool registration failed
- Agent wants explicit control over routing

### 5. Security: trust model

- **Global Hub:** User explicitly registers = trusted by default
- **Project Hub:** Capabilities with `type=mcp` default to `enabled=false`, require user approval
- **Env inheritance:** Child processes get explicit env from definition, not full parent env
- **Provenance:** Track who/when registered each capability

### 6. Manual ServerHandler

Replace `#[tool(tool_box)]` macro-generated dispatch with manual `ServerHandler` implementation. This enables runtime-dynamic tool list from Hub + proxy pool alongside the compile-time native tools.

## Implementation

### Files to modify

| File | Changes |
|------|---------|
| `crates/memory-server/Cargo.toml` | Add `client`, `transport-child-process`, `transport-sse` features |
| `crates/memory-server/src/main.rs` | McpClientPool, manual ServerHandler, enhanced hub_register, hub_call tool |

### Data structures

```rust
struct ChildConnection {
    peer: Peer<RoleClient>,
    tools: Vec<Tool>,
    last_used: Instant,
    active_calls: AtomicU32,
    state: ConnectionState,
}

enum ConnectionState {
    Ready,
    Unhealthy { retry_after: Instant },
}

struct McpClientPool {
    connections: DashMap<String, Arc<Mutex<ChildConnection>>>,
    definitions: DashMap<String, HubCapability>,
    cached_tools: DashMap<String, Vec<Tool>>,
    idle_ttl: Duration,
}
```

### Enhanced hub_register for type=mcp

```rust
async fn hub_register(&self, params) -> Result<String, String> {
    if params.cap_type == "mcp" {
        // Parse definition for transport config
        let def: McpServerDef = serde_json::from_str(&params.definition)?;

        // Connect, discover tools, cache, disconnect
        let tools = self.discover_child_tools(&def).await?;

        // Store tools in definition
        let mut full_def = serde_json::from_str::<Value>(&params.definition)?;
        full_def["discovered_tools"] = serde_json::to_value(&tools)?;
        params.definition = serde_json::to_string(&full_def)?;
    }

    // ... existing register logic
}
```

### Manual ServerHandler

```rust
impl ServerHandler for MemoryServer {
    async fn list_tools(&self, _req) -> Result<ListToolsResult, McpError> {
        let mut tools = self.native_tool_list();  // 15 native tools

        // Add proxy tools from all registered MCP servers
        for entry in self.client_pool.cached_tools.iter() {
            let server_id = entry.key();
            for tool in entry.value() {
                let mut proxied = tool.clone();
                proxied.name = format!("{}__{}", server_id, tool.name);
                tools.push(proxied);
            }
        }

        Ok(ListToolsResult { tools })
    }

    async fn call_tool(&self, req) -> Result<CallToolResult, McpError> {
        let name = &req.params.name;

        // Try native tools first
        if let Some(result) = self.try_native_call(name, &req.params).await {
            return result;
        }

        // Parse proxy tool name: "github__create_pr" → ("github", "create_pr")
        if let Some((server_id, tool_name)) = name.split_once("__") {
            return self.proxy_call(server_id, tool_name, req.params.arguments).await;
        }

        Err(McpError::method_not_found())
    }
}
```

### MCP Server Definition format

```json
{
  "transport": "stdio",
  "command": "gh-mcp-server",
  "args": ["--token", "${GITHUB_TOKEN}"],
  "env": {
    "GITHUB_TOKEN": "..."
  },
  "max_concurrency": 1,
  "idle_ttl_ms": 300000,
  "startup_timeout_ms": 10000,
  "tool_timeout_ms": 30000
}
```

For SSE:
```json
{
  "transport": "sse",
  "url": "https://mcp.linear.app/mcp",
  "headers": {
    "Authorization": "Bearer ${LINEAR_API_KEY}"
  }
}
```

## Testing

### Unit tests
- McpClientPool connection lifecycle
- Tool name prefixing/parsing
- Schema caching round-trip

### Integration tests (update test_memory_server.py)
- `hub_register(type=mcp, ...)` with a test MCP server → verify tools discovered
- `tools/list` includes proxy tools
- Call a proxy tool → verify forwarding works
- Idle cleanup → verify child process terminated
- `hub_call(server, tool, args)` fallback

### Test MCP server
Create a minimal test MCP server (Python or Rust) that exposes 2-3 simple tools for integration testing.

## Risks and mitigations

| Risk | Mitigation |
|------|-----------|
| Stale cached schemas | Background refresh on startup; explicit "capability changed" error |
| Child server crashes | Health tracking + reconnect with backoff |
| Tool list explosion | Only expose enabled capabilities; cap total |
| Security (arbitrary exec) | Trust model: global=trusted, project=disabled-by-default |
| rmcp macro removal | Keep native tool logic in methods; only change dispatch layer |

## Not in scope (Phase 3+)

- Automatic tool schema refresh/polling
- Cross-agent tool sharing optimization
- Tool composition/chaining
- Evolution of MCP server usage patterns
- NAPI binding for proxy tools (all agents use MCP path)
