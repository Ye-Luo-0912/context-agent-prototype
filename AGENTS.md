# Agent Development Contract

This repository is an experimental coding-agent runtime whose primary research target is **continuous context lifecycle management**.

Before changing architecture, preserve the invariants below.

## Non-negotiable invariants

1. **Do not make the conversation transcript the source of model context.**
   - Model input must be rebuilt from `ContextEngine::materialize`
     (`ContextQuery` -> structured `MaterializedContext`), then rendered by
     the runtime-owned `PromptAssembler`.
   - Never add a global append-only `messages: Vec<_>` to `AgentKernel` as the authoritative context mechanism.

2. **Token pressure must not be the trigger for forgetting.**
   - Context maintenance runs on runtime events (`UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`, `TaskCompleted`, `Checkpoint`).
   - Budgeting is only final packing.

3. **Tools must not access ContextEngine or memory stores.**
   - `tool-runtime` may depend on `agent-contracts` and workspace facilities.
   - Tool results return through `ToolOutput` to the kernel.
   - The kernel/context policy decides whether the observation enters context.

4. **Raw tool output is not prompt history.**
   - Large outputs go to `.focus-agent/artifacts/...`.
   - `ToolOutput::model_content` is explicitly bounded.

5. **Context implementation is replaceable.**
   - `agent-kernel` must depend only on the `ContextEngine` contract.
   - Do not import `context-simple` from the kernel.
   - Future ContextCore integration must be a new adapter/implementation, not a kernel rewrite.

6. **UI consumes runtime events.**
   - Do not bind TUI widgets directly to mutable kernel/context internals.
   - Grow `AppState` toward a reusable `RunStateAggregator`/view model.

7. **Raw traces are filesystem artifacts.**
   - Runtime traces/learning data use JSONL/artifact files in the prototype.
   - Do not add a database just to store raw traces.

8. **Keep v0 non-vector.**
   - No embedding/vector DB/RAG/graph retrieval until the dynamic working-set baseline is measured.

## Dependency rules

Allowed high-level direction:

```text
agent-contracts
  ^
  +-- context-simple
  +-- agent-workspace
  +-- tool-runtime (also -> agent-workspace)
  +-- agent-storage
  +-- agent-kernel

agent-runtime -> agent-kernel + agent-workspace
agent-tui -> composition of all implementations
```

`agent-kernel` is the **stateless core facade** (contracts, budgets,
approval, event publication, model/tool/context wiring through traits —
no turn state). `agent-runtime` owns the turn state machine (`RuntimeActor`),
the scope lifecycle, prompt assembly, capability registry and module host,
and is the only orchestrator. Keep it that way: never add turn state back to
the kernel, and never let a second orchestrator appear. The long-term
direction is to fold the kernel's remaining responsibilities into
`agent-runtime` and retire the kernel crate — but only by moving them, not
by resurrecting a parallel orchestrator.

The runtime stays implementation-agnostic: concrete context engines
(`context-simple`, `context-baselines`) and tool dispatchers (`tool-runtime`)
are wired by the composition root (`agent-tui`), never imported by
`agent-runtime`.

Forbidden examples:

```text
agent-kernel -> context-simple       # forbidden
agent-kernel -> agent-tui            # forbidden
agent-runtime -> context-simple      # forbidden (concrete engine)
agent-runtime -> tool-runtime        # forbidden (concrete dispatcher)
tool-runtime -> context-simple       # forbidden
tool-runtime -> ContextEngine        # forbidden
context-simple -> tool-runtime       # forbidden
agent-contracts -> concrete crate    # forbidden
```

## Context policy development

Before making the selection algorithm smarter, improve observability first.

Every context policy change should eventually be explainable as:

```text
item entered because ...
item selected because ...
item cooled/archived/dropped because ...
item evicted because ...          # GC pass: roots marked, why it left
item reactivated because ...      # GC pass: why it came back
model turn N consumed it
```

Since V1-M9 the GC dimensions are explicit and orthogonal:
`ContextItem` carries `attention` (Active/Cooling/Archived), `semantic`
(Live/Superseded/VerifiedFixed/Tombstoned — terminal states never
resurrect), `residency` (Resident/Warm/Cold/External) and `gc_generation`,
and `ContextEngine::gc()` runs a mark/roots + sweep + reversible-eviction
pass that reports a reason for every eviction and reactivation. Eviction
is store-backed, never destructive: a full buffer writes the oldest
eviction to the `ContextStore` as an `ExternalizedContext` (`ContextRef` +
artifact links) instead of purging it; permanent deletion is reserved for a
future storage GC with conservative triggers (no live references + retention
expiry + non-audit + not pinned/durable).

Prefer explicit features before opaque learned scoring:

- task/focus affinity;
- scope;
- retention;
- recency;
- access reinforcement;
- file/symbol/entity affinity;
- dependency/supersession relationships;
- verified error/fix status.

## Performance rules

- Keep database/network work out of the context hot path unless measurement proves it necessary.
- Use bounded channels and bounded model-facing tool output.
- Do not clone large tool results into events/context; store them once as artifacts.
- Do not optimize with `unsafe` before profiling shows a real hotspot.
- Streaming process output should use a bounded/ring buffer when implemented.
- Event persistence should remain buffered/off the runtime hot path.

## First implementation priorities

See `docs/ROADMAP.md`. Current state at this head:

- done: P0 skeleton; P0.5 observability + deterministic replay; P1 real
  streaming provider; P2 backlog (retry safety, cancellable backoff,
  bounded provider errors, per-provider wire flags); V1-M3 runtime
  framework; V1-M6 context GC v1; V1-P0..P8 (actor owns the turn,
  prompt split, scope ownership, extension platform, workspace path
  confinement, mutation transaction, model budget, shutdown ownership,
  tool lifecycle, capability maturity ladder, capability manifest +
  process transport hardening); V1-M9 part 1 (attention/semantic state
  split with tombstones that never resurrect, dependency-mark direction
  fixed, store-backed eviction via `ContextEngine::store`/`restore`
  instead of purge, GC roots tightened to not include the whole active
  task, scope-close promote-before-archive with `scope_id` kept in sync,
  `ToolSurfaceSnapshot` per model round, unified tool/capability catalog
  with one `ToolLifecycle` and unified `capability.search/inspect/load/
  unload`); V1-M9 part 2 (adaptive runtime: `context.gc_hint` / `tag` /
  `lease` / `collect` meta-tools, `ToolOutput.context_action` +
  `ContextIngress::ContextDirective`, item `keep_alive` / `lease_until_turn`
  GC roots with buffer reactivation and consumed-ephemeral override,
  collect routed to a mid-turn `ContextEngine::gc()` with the report
  emitted as a `ContextGc` event).

Next, in order:

1. V2 Self-Iteration — capability generation → sandbox test → replay →
   evaluate → register/rollback. The LLM grows capabilities instead of
   editing production core.
2. Evidence-gated (later, only after measurement): smarter non-vector
   lifecycle policy, ContextCore adapter, vector recall / learned selection
   / cross-session memory (invariant 8).

Every architectural change must still be explainable as `entered because /
selected because / cooled-archived-dropped because / evicted-reactivated
because / model turn N consumed it`.

## Definition of done for architectural changes

An architectural change is incomplete unless:

- dependency direction still satisfies this file;
- the new behavior has a bounded-output policy;
- runtime events expose enough state to debug it;
- a test covers the new context/tool lifecycle behavior;
- `docs/ARCHITECTURE.md` or `docs/CONTEXT_LIFECYCLE.md` is updated when the contract changes.
