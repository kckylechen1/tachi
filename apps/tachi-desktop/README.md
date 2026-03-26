# tachi-desktop

Desktop shell for Tachi Computer app.

## Current Scope

- daemon health + connection status
- memory search / inspect
- ghost channels / agent graph / kanban views
- hub dashboard + audit / GC snapshots

## Current Transport Baseline

- daemon default: `http://127.0.0.1:6919/mcp`
- dev proxy: `/tachi/mcp` -> `http://127.0.0.1:6919/mcp`
- renderer speaks Streamable HTTP MCP via JSON-RPC

## Status

- usable prototype: yes
- installer / governance / rollout workflow: not yet
- Virtual Capability UI: not yet

## Dev

```bash
npm install
npm run dev
```

In another shell:

```bash
cargo run -p memory-server -- --daemon --port 6919
```

For single-project daemon mode:

```bash
cargo run -p memory-server -- --daemon --port 6919 --project-db /abs/path/to/repo/.tachi/memory.db
```
