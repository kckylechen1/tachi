# Kernel Surface V1

## Purpose

`Tachi` is the kernel. `Kernel Surface V1` defines the minimal, opinionated contract that the kernel exposes to hosts and agents.

The goal is not to expose every primitive. The goal is to expose the right layers:

1. kernel
2. capability
3. runtime
4. workflow
5. admin

The core boundary is:

`Tachi does not need to own all execution tools; it needs to understand, recommend, and orchestrate them.`

## Layer 1: Kernel

These are the durable state and memory primitives that define the kernel itself.

- memory
- fact
- edge
- state
- section
- compact artifacts

Examples:

- `memory_search`
- `memory_save`
- `memory_get`
- read-only graph lookups
- future `section.build`
- future `compact.rollup`
- future `compact.session_memory`

This layer owns the canonical memory graph and its maintenance lifecycle.

## Layer 2: Capability

This is the missing “librarian brain” layer.

It should answer:

- which skill is best for this task
- which host tools are appropriate
- which toolchain has worked before
- which pack or extension should be activated

Candidate APIs:

- `recommend_capability`
- `recommend_skill`
- `recommend_toolchain`
- future `prepare_capability_bundle`

Status:

- first-pass recommendation APIs are now implemented
- current implementation is deterministic and Hub/Pack-aware
- future iterations can add richer outcome learning and LLM-assisted planning on top

This layer does not execute the host’s tools directly. It selects and orchestrates them using:

- memory
- tooluse history
- host constraints
- prior outcomes
- profile and policy

## Layer 3: Runtime

These are host/runtime-facing primitives used by adapters and hooks, not by default model-facing tool lists.

Examples:

- `recall_context`
- `capture_session`
- `compact_context`

Runtime primitives are called by:

- `before_agent_start`
- `agent_end`
- future `before_compaction`
- other host lifecycle hooks

This layer is where OpenClaw and other adapters talk to Tachi during a live turn.

## Layer 4: Workflow

These are higher-order coordination and reflective flows.

Examples:

- `ghost_*`
- kanban / delegation
- handoff
- evolution review / projection flows

These should be:

- hidden by default
- profile-gated
- exposed only to the right host or operator flow

The key rule is that workflow tools should not be mixed into the default kernel surface.

## Layer 5: Admin

These are operator and system management capabilities.

Examples:

- hub
- vault
- sandbox
- pack
- vc
- dlq

These stay out of the normal agent-facing surface.

## Host Boundary

### Tachi

Owns:

- memory graph
- extraction
- embedding
- reranking
- distillation
- forgetting / archival
- capability recommendation
- skill evolution
- agent evolution
- profile projection

### Host adapters

Own:

- runtime timing
- lifecycle hooks
- final context assembly
- token-pressure decisions
- exposure policy
- host-native execution tool invocation

### Host-native tools

Host-native execution tools are not part of the Tachi kernel surface, but they are part of the capability model.

Examples:

- shell
- browser
- python
- filesystem
- excel or spreadsheet tooling

Tachi should know about them well enough to recommend and orchestrate them, even when the host owns actual execution.

## Default Exposure Model

### Agent-facing

- `memory_search`
- `memory_save`
- `memory_get`

### Runtime-only

- `recall_context`
- `capture_session`
- `compact_context`

### Workflow-gated

- `ghost_*`
- kanban / delegation / handoff
- evolution review / projection

### Admin-only

- hub
- vault
- sandbox
- pack
- vc
- dlq

### Capability-facing

First-pass implementation is now available for direct MCP hosts and operator flows.

- `recommend_capability`
- `recommend_skill`
- `recommend_toolchain`

## Why This Matters

Without the capability layer, Tachi becomes:

- a big MCP server with too many raw tools
- a strong memory kernel with no recommendation brain

With the capability layer, Tachi becomes:

- memory kernel
- capability catalog
- recommendation engine
- orchestration brain

That is the intended direction for `Neural Foundry`.
