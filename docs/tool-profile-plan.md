# Tool Profile & Host Surface Plan

This document is the exposure-policy companion to [Kernel Surface V1](./kernel-surface-v1.md).  
`Kernel Surface V1` defines the conceptual layers; this document defines how much of that surface each host sees by default.

## Goal

Keep `Tachi` as the full kernel, but stop showing the full kernel to every host and every agent.

The target split is:

- `Tachi`
  - owns kernel, capability, runtime, workflow, and admin primitives
- `OpenClaw`
  - owns hooks, runtime timing, section assembly, and agent-facing tool exposure
- `IDE / direct MCP clients`
  - see a small host-appropriate tool subset instead of the full 60+ tool catalog

## Principles

1. Agent-facing tools stay tiny.
   - Default agent surface should be `memory_search`, `memory_save`, and `memory_get`
   - Memory graph access should stay read-only when we add a higher-level graph tool
2. Runtime hooks stay internal.
   - `recall_context`, `capture_session`, and later `compact_context` are host/runtime APIs, not default agent tools
3. Capability selection should become a first-class public layer.
   - `recommend_capability`, `recommend_skill`, and `recommend_toolchain` belong in the kernel surface
   - but they should be added as a deliberate layer, not mixed into low-level memory or admin tools
4. Workflow tools are not kernel primitives.
   - `ghost_*`, `post_card`, `check_inbox`, `update_card`, proposal review/project tools stay hidden unless a host or profile explicitly asks for them
5. Filtering must only reduce exposure.
   - Effective surface is the intersection of:
     - built-in host profile
     - `TACHI_EXPOSED_TOOLS`, if present
   - We should not let one layer widen another
6. `agent_register.tool_filter` is deferred.
   - The current in-process `agent_profile` state is shared too broadly for daemon-safe per-session tool filtering
   - Host-level profile selection is safe now; per-session runtime filters come later

## Implemented

### Built-in host profiles

`Tachi` now understands four built-in profiles:

- `ide`
  - `recommend_capability`
  - `recommend_skill`
  - `recommend_toolchain`
  - `search_memory`
  - `save_memory`
  - `get_memory`
  - `list_memories`
  - `memory_stats`
  - `get_edges`
- `runtime`
  - `ide` +
  - `recall_context`
  - `capture_session`
  - `archive_memory`
  - `delete_memory`
  - `extract_facts`
  - `find_similar_memory`
  - `get_pipeline_status`
  - `ingest_event`
  - `sync_memories`
- `workflow`
  - `ghost_*`
  - `handoff_*`
  - `post_card`
  - `check_inbox`
  - `update_card`
  - evolution proposal/project tools
- `admin`
  - full surface

Selection paths:

- `tachi --profile ide`
- `TACHI_PROFILE=runtime tachi`
- default with no profile: `admin`

### OpenClaw extension surface

The OpenClaw plugin now exposes only:

- `memory_search`
- `memory_save`
- `memory_get`

It no longer exposes raw passthrough workflow tools like:

- `tachi_ghost_publish`
- `tachi_kanban_post`
- `tachi_save_memory`
- `memory_hybrid_search`

Internally, the plugin still uses runtime-only MCP primitives through hooks:

- `before_agent_start` → `recall_context`
- `agent_end` → `capture_session`

OpenClaw also forces `TACHI_PROFILE=runtime` when it launches the embedded MCP client.

### Compaction primitive

`Tachi` now exposes `compact_context` as a runtime-only API.

- Input
  - `agent_id`
  - `conversation_id`
  - `window_id`
  - `messages`
  - token-budget hints
- Output
  - `compacted_text`
  - `estimated_tokens`
  - topic/signal summaries for later section work

Today this is a typed MCP/runtime primitive, not an OpenClaw hook integration yet. The current OpenClaw SDK only exposes `before_agent_start` and `agent_end`, so the actual `before_compaction` wiring is deferred until the host exposes that lifecycle event.

### Capability recommendation primitive

`Tachi` now exposes a first-pass capability layer:

- `recommend_capability`
- `recommend_skill`
- `recommend_toolchain`

Current behavior:

- deterministic ranking over Hub capabilities
- visibility/callability aware
- host-aware scoring
- Pack / projection-aware toolchain suggestions
- simple host-tool inference for common task shapes

OpenClaw does not expose these directly to the model yet. They are part of the kernel surface for direct MCP hosts and future adapter orchestration.

## Why This Split

This keeps the model-facing surface small while preserving a rich kernel for:

- OpenClaw hooks
- IDE integrations
- future capability recommendation
- future compaction APIs
- operator and workflow tooling

It also preserves compatibility for direct MCP clients that do not have a native extension layer: they can still select a smaller host profile without losing the full kernel from `admin`.

## Next Steps

1. Add section/compaction artifacts
   - `section.build`
   - `compact.rollup`
   - `compact.session_memory`
2. Wire OpenClaw into `compact_context`
   - as soon as the SDK exposes a pre-compaction lifecycle hook
3. Harden the capability layer
   - add richer outcome signals
   - connect more pack / projection metadata
   - add `prepare_capability_bundle`
4. Wire cron to queued evolution
   - OpenClaw cron triggers Tachi evolution jobs
   - Tachi produces proposals and projection targets
5. Revisit per-session filtering
   - Move runtime tool filters off shared server state before extending `agent_register`

## Verification

```bash
cargo test -p memory-server
npm --prefix integrations/openclaw run build
```
