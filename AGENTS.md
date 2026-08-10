# Agent Development Contract

This repository is an experimental coding-agent runtime whose primary research target is **continuous context lifecycle management**.

Before changing architecture, preserve the invariants below.

## Working principles (apply to every code change)

1. **Understand the code before changing it.** Read the module, its callers
   and its tests first; say explicitly what behavior is being preserved and
   what is being changed. Never edit code whose current semantics you cannot
   explain.
2. **Reuse before writing.** Prefer the existing abstraction; extract small,
   composable primitives instead of copying logic; keep public APIs minimal
   and documented. Do not hard-code one caller's assumptions into a shared
   function.
3. **High performance, measured.** Keep work off the hot path and large
   results out of clones (see Performance rules); only optimize what
   profiling proves hot; never regress the runtime's boundedness or
   latency for the sake of cleverness.
4. **Maintainable.** Small, single-purpose functions; clear names; explicit
   comments explaining *why*; a reader should be able to explain any line's
   purpose and any test's failure without reverse-engineering the whole
   system.

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
  +-- agent-process        (framed IPC, child lifecycle, sandbox hooks)
  +-- agent-kernel

agent-capability-process -> agent-process (+ agent-contracts)
context-contextcore      -> agent-process (+ agent-contracts)
agent-runtime -> agent-kernel + agent-workspace
agent-tui -> composition of all implementations
```

`agent-kernel` is the **stateless core facade** (contracts, budgets,
approval, event publication, model/tool/context wiring through traits —
no turn state). `agent-runtime` owns the turn state machine (`RuntimeActor`),
the scope lifecycle, prompt assembly, capability registry and module host,
and is the only orchestrator. Keep it that way: never add turn state back to
the kernel, and never let a second orchestrator appear.

The long-term direction is a **Trusted Core**, not a merged-then-retired
kernel: as self-iteration, process capabilities, effect commit, permissions,
sandboxing and evaluation land, the system needs a core the agent cannot
modify. The kernel's stateless primitives (permission/approval, effect
brokering, event/audit/durability, resource budgets, capability authority,
sandbox authority, runtime integrity) grow into `agent-core`; everything
evolvable (the `RuntimeActor`, task manager, scope scheduling, prompt
assembly, tool/context materialization, adaptive policy) stays in
`agent-runtime`. **Runtime evolves, Core stays trusted** — the fold is into
`agent-core`, never into the orchestrator, and never by resurrecting a
parallel orchestrator.

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
  emitted as a `ContextGc` event); V1-M9 part 3 (merged control surface
  `fs.list`/`fs.read`/`search.grep`/`context.manage`/`capability.manage`
  with paged, artifact-spilling search, registration caps for
  capability-declared tool schemas, input-budget final guard that
  auto-unloads optional tools instead of shrinking the prompt, capability
  registry surface generation, validated manifest/tool-spec caching at
  registration); V1-M10 first step (per-capability async lifecycle state
  machine — Stopped/Starting/Started/Stopping/Failed with serialized
  start/stop, so concurrent `ensure_started` calls never double-start;
  process host split into `agent-process` (host/framing/sandbox) +
  `agent-capability-process` (adapter) with `context-contextcore` reduced
  to the ContextEngine adapter; `StorageGc` wire op with a full-contract
  process-boundary parity test that exercises every `ContextEngine` method;
  turn finalization as a durable commit — Running → ModelFinished →
  Committing → Committed, `TurnCompleted` only after every mandatory state
  write, `TurnCommitFailed`/`RecoveryRequired` on the first failure).

Next, in order (M12/M13 strictly before Self-Iteration):

1. V1-M10 Runtime Consistency — task authority, transactional task
   transitions, RuntimeCheckpoint, Turn commit. Acceptance: the runtime and
   the context never drift into a task/state split-brain.
2. V1-M11 Context Recall — store injection, ContextMapView,
   `context.search`/`fetch`, `gc_epoch`, async store. Acceptance: external
   information can be pulled back on demand without polluting the prompt.
3. V1-M12 Effect Runtime — every capability routes side effects through one
   unified EffectRequest/Effect commit. Acceptance: a cancelled operation
   produces no avoidable stale mutation.
4. V1-M13 Extension Sandbox — process sandbox, env scrub, brokered
   FS/network, cancel. Acceptance: experimental code cannot exceed the
   permissions granted to it.
5. V1-M14 Resource Policy — tool schema budget, context hint quota,
   RiskClass, PermissionSet. Acceptance: the LLM cannot exhaust runtime
   resources through meta-tools.
6. V1-M15 Real Evaluation — coding workload A/B/C + lifecycle metrics.
   Acceptance: the dynamic runtime saves tokens without lowering task
   success rate.
7. V2 Self-Iteration — generate → sandbox → test → replay → evaluate →
   canary → stable. The LLM grows capabilities, but cannot modify the
   evaluation or permission Core.
8. Evidence-gated (later, only after measurement): smarter non-vector
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
