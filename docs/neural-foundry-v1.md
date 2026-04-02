# Neural Foundry V1

## Why

Tachi is moving from a memory-centric MCP server toward a shared runtime for:

- memory capture and retrieval
- skill optimization
- agent/profile evolution
- cross-host projection

The core rule is simple:

- business logic lives in Tachi
- MCP / CLI / Agent SDK expose interfaces
- host adapters stay thin

OpenClaw is the first migration target, not the final architecture.

## Boundaries

### Tachi Core

Owns the canonical source of truth for:

- evidence
- memories
- skills
- agent profiles
- evolution proposals
- projection state

Owns the heavy model work:

- embedding
- extraction
- reranking
- distillation
- forgetting / archival proposals
- skill evolution
- agent evolution

### Interfaces

Tachi may be called from:

- MCP
- CLI
- Agent SDK adapters
- future HTTP / daemon surfaces

These are transport layers, not the business layer.

### Adapters

Hosts such as OpenClaw, Claude Code, and Codex should only:

- capture lifecycle events
- submit evidence
- request recall / proposals
- project canonical profile data into host-native files or config

## Runtime Shape

### Online path

Latency-sensitive calls used during active agent turns:

- `recall_context`
- `memory_search`
- `capture_session`

### Offline path

Worker-driven jobs that should not block tool use:

- `memory_rerank`
- `memory_distill`
- `forget_or_archive`
- `skill_evolution`
- `agent_evolution`
- `profile_projection`

## Current Status

### Implemented in `feat/neural-foundry`

The branch has already landed the first Foundry foundations and the first online migration slice.

Completed:

- canonical Foundry schema in `memory-core`
- manual `synthesize_agent_evolution` tool
- `recall_context` online API in Tachi
- `capture_session` online API in Tachi
- Tachi-side rerank for recall results
- Tachi-side extraction + embedding for captured session memories
- first Foundry maintenance worker for `memory_rerank`, `memory_distill`, and `forget_sweep`
- OpenClaw integration updated to prefer the new Tachi APIs

This means OpenClaw now prefers:

- `recall_context` for `before_agent_start`
- `capture_session` for `agent_end`
- queued maintenance jobs for post-capture memory upkeep

### Still using compatibility fallback

The migration is intentionally partial right now.

OpenClaw still keeps local fallback paths for:

- local FTS search

Tachi still keeps some inline maintenance in the online path for safety:

- inline capture dedup / merge before worker handoff

That remaining compatibility layer is temporary and exists only to keep the plugin usable while the Tachi-side worker pipeline stabilizes.

## Canonical Data

### Evidence

Evidence is broader than memory. It includes:

- memories
- reflections
- tooluse traces
- eval logs
- ghost messages
- session outcomes
- skill telemetry
- profile snapshots

### Agent profile

`AGENTS.md` and `IDENTITY.md` are projection targets, not the source of truth.

The canonical profile should capture:

- identity / mutable traits
- routing rules
- tool policy
- memory policy
- model policy
- evolution proposals

### Projection

Projection writes host-native artifacts such as:

- `AGENTS.md`
- `IDENTITY.md`
- tool / routing config
- cron specs

Projection must be section-aware. It should update managed sections instead of overwriting whole files.

## Migration Plan

### Phase 1

Stand up the Neural Foundry foundations in Tachi:

- canonical types
- synthesis prompts
- manual evolution synthesis entrypoint

Status:

- completed

### Phase 2

Move OpenClaw memory logic into Tachi:

- extractor
- embedding
- reranker
- dedup / merge

OpenClaw becomes a thin lifecycle adapter.

Status:

- in progress

Already done:

- online recall moved behind `recall_context`
- session capture moved behind `capture_session`
- OpenClaw now prefers Tachi-side online APIs
- OpenClaw `agent_end` delegates to `capture_session` instead of doing local model calls
- failed capture windows are spooled locally and replayed on the next healthy Tachi connection
- inline dedup / merge now runs inside Tachi `capture_session`
- `recall_context` can auto-scope by `agent_id`, and OpenClaw now passes it through
- OpenClaw `before_agent_start` uses `recall_context` first, with local FTS fallback only for resilience
- user-initiated OpenClaw memory search now also degrades only to local FTS fallback instead of local embedding + rerank
- `capture_session` now queues Foundry maintenance jobs for `memory_rerank`, `memory_distill`, and `forget_sweep`
- Foundry maintenance now tracks worker counters through `get_pipeline_status`
- `recall_context` now enforces agent-scoped path policy server-side
- default agent recall now pulls from both live agent memories and Foundry distill memories

Still missing:

- remove or further minimize local recall fallback in OpenClaw
- move more inline dedup / merge and maintenance decisions into the offline worker pipeline
- harden distill / forget policies beyond the current first-pass worker implementation

### Phase 3

Add workerized evolution:

- reflection synthesis
- memory distillation
- forgetting / archival proposals
- skill evolution
- agent evolution proposals

Status:

- foundations in progress

### Phase 4

Add projection engine:

- write approved proposals into host-native artifacts
- sync profile changes across adapters

Status:

- not started

## First Deliverable

The first deliverable in `feat/neural-foundry` is intentionally small:

- define canonical Foundry types
- add a manual `synthesize_agent_evolution` entrypoint
- return structured proposals without changing any host files yet

That keeps the migration reversible while the schema stabilizes.

## Next Steps

The next implementation slice should stay focused on finishing the OpenClaw-first migration before expanding the architecture.

### Next slice

- shrink or remove the remaining local recall fallback in OpenClaw
- move more capture maintenance out of the online path and into Foundry workers
- harden `memory_distill` and `forget_sweep` policies with better scoring and archival rules

### After that

- add `agent_evolution` worker jobs that consume profile docs, reflections, evals, and tooluse evidence
- add proposal storage and approval gates
- add section-aware projection back into `AGENTS.md`, `IDENTITY.md`, and policy files

### Exit criteria for the current migration stage

- OpenClaw no longer needs local extractor / reranker / merge logic for the normal capture path
- Tachi owns the normal online memory capture flow end-to-end
- failed capture windows are durable across temporary Tachi outages
- post-capture maintenance is queued into Foundry workers instead of living entirely inside `capture_session`
- local recall fallback remains acceptable only as a resilience path, not the primary path
