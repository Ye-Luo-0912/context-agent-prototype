# Agent Development Contract

This repository is an experimental coding-agent runtime focused on
**continuous context lifecycle management**. Preserve this contract before
changing architecture.

## Working rules

1. **Understand before editing.** Read the module, callers, and tests; state
   what behavior stays and what changes. Do not edit semantics you cannot
   explain.
2. **Reuse before adding.** Prefer existing abstractions and small composable
   primitives. Keep public APIs minimal, documented, and caller-agnostic.
3. **Keep performance bounded and measured.** Keep expensive work off hot
   paths and large values out of clones. Optimize only measured hotspots; do
   not trade boundedness or latency for cleverness.
4. **Keep code maintainable.** Use single-purpose functions, clear names, and
   comments that explain *why*. Tests and failures should be understandable
   without reverse-engineering the system.

## Non-negotiable invariants

1. **The transcript is not the source of model context.** Rebuild model input
   through `ContextEngine::materialize(ContextQuery) -> MaterializedContext`,
   then render it with the runtime-owned `PromptAssembler`. Never add an
   authoritative global `messages: Vec<_>` to `AgentKernel`.
2. **Token pressure does not trigger forgetting.** Maintenance is driven by
   `UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`,
   `TaskCompleted`, and `Checkpoint`; budgeting is final packing only.
3. **Tools cannot access context engines or memory stores.** Tools return
   `ToolOutput`; kernel/context policy decides what enters context.
   `tool-runtime` may depend only on contracts and workspace facilities.
4. **Raw tool output is not prompt history.** Store large output once under
   `.focus-agent/artifacts/...`; keep `ToolOutput::model_content` bounded.
5. **Context implementations are replaceable.** `agent-kernel` depends only
   on `ContextEngine`; ContextCore must arrive as an adapter/implementation,
   never a kernel rewrite or a `context-simple` import.
6. **UI is event-driven.** UI code consumes runtime events, not mutable
   kernel/context internals. Evolve `AppState` toward a reusable
   `RunStateAggregator`/view model.
7. **Raw traces are filesystem artifacts.** Use JSONL/artifact files; do not
   add a database merely to store traces or learning data.
8. **Keep v0 non-vector.** No embeddings, vector DB, graph retrieval, or RAG
   until the dynamic working-set baseline is measured.

## Architecture and dependencies

Allowed high-level direction:

```text
agent-contracts
  ^
  +-- context-simple
  +-- agent-workspace
  +-- tool-runtime (also -> agent-workspace)
  +-- agent-storage
  +-- agent-process (framed IPC, child lifecycle, sandbox hooks)
  +-- agent-kernel

agent-capability-process -> agent-process (+ agent-contracts)
context-contextcore      -> agent-process (+ agent-contracts)
agent-runtime            -> agent-kernel + agent-workspace
agent-tui                -> composition of implementations
```

- `agent-kernel` is the **stateless Core facade**: contracts, budgets,
  approval, events/audit/durability, and model/tool/context wiring through
  traits. It owns no turn state.
- `agent-runtime` is the **only orchestrator**. It owns `RuntimeActor`, task
  and scope lifecycle, prompt assembly, capability registry, module host, and
  adaptive policy. Never put turn state back in the kernel or add a second
  orchestrator.
- Concrete context engines (`context-simple`, `context-baselines`) and tool
  dispatchers (`tool-runtime`) are wired only by the composition root
  (`agent-tui`); runtime remains implementation-agnostic.
- Long term, kernel primitives grow into an agent-modification-resistant
  `agent-core`: permissions/approval, effect brokering, audit/durability,
  budgets, capability/sandbox authority, and runtime integrity. Evolvable
  orchestration stays in `agent-runtime`; the fold is into `agent-core`, never
  the orchestrator or a parallel orchestrator. **Runtime evolves; Core stays
  trusted.**

Forbidden dependencies:

```text
agent-kernel  -> context-simple | agent-tui
agent-runtime -> context-simple | tool-runtime
tool-runtime  -> context-simple | ContextEngine
context-simple -> tool-runtime
agent-contracts -> any concrete crate
```

## Context and GC policy

Improve observability before making selection smarter. Every policy change
must eventually explain:

```text
item entered because ...
item selected because ...
item cooled/archived/dropped because ...
item evicted because ...
item reactivated because ...
model turn N consumed it
```

Keep lifecycle dimensions orthogonal:

- `attention`: `Active | Cooling | Archived`;
- `semantic`: `Live | Superseded | VerifiedFixed | Tombstoned` (terminal
  states never resurrect);
- `residency`: `Resident | Warm | Cold | External`;
- generation/retention/access metadata remains separate from all three.

`ContextEngine::gc()` is mark/roots + sweep + reversible eviction and must
report reasons. Context GC never destroys information: buffer overflow writes
the oldest eviction to `ContextStore` as `ExternalizedContext`/`ContextRef`.
Only conservative Storage GC may delete data, and only when it has no live
references, retention has expired, and it is neither audit nor
pinned/durable.

Prefer explicit, explainable features before learned/opaque scoring:
task/focus affinity, scope, retention, recency, access reinforcement,
file/symbol/entity affinity, typed dependencies/supersession, and verified
error/fix state.

## Performance rules

- Keep database/network work out of context hot paths unless measurement
  proves it necessary.
- Bound channels, decoded frames, model-facing output, events, and previews.
- Store large tool results once; do not clone them into events/context.
- Use bounded/ring buffers for streaming process output.
- Keep event persistence buffered and off the runtime hot path while
  preserving required durability barriers.
- Do not use `unsafe` without a profiled hotspot and explicit justification.

## Delivery order

`docs/ROADMAP.md` is the milestone authority; `docs/AUDIT_TODO.md` tracks
confirmed defects; context/tool design queues live in their dedicated TODOs.
The current baseline already includes the actor-owned runtime, structured
prompt/scope model, observable store-backed GC, bounded context/tool controls,
process adapters, runtime checkpoints, and durable turn commits. Do not copy
their detailed status into this file.

Keep this order; M12 and M13 must finish before Self-Iteration:

1. **V1-M10 Runtime Consistency:** task authority, transactional transitions,
   checkpoints, and turn commit. Runtime and context must never split-brain.
2. **V1-M11 Context Recall:** store injection, bounded `ContextMapView`,
   search/fetch, `gc_epoch`, and async store. Recall must not pollute prompt
   history.
3. **V1-M12 Effect Runtime:** route every capability side effect through one
   `EffectRequest`/commit path. Cancellation must avoid stale mutation.
4. **V1-M13 Extension Sandbox:** scrub environment; broker filesystem/network;
   enforce cancellation and permissions.
5. **V1-M14 Resource Policy:** bound tool schemas, context hints, `RiskClass`,
   and `PermissionSet` so meta-tools cannot exhaust runtime resources.
6. **V1-M15 Real Evaluation:** coding-workload A/B/C plus lifecycle, cost, and
   latency metrics. Save tokens without reducing task success.
7. **V2 Self-Iteration:** generate -> sandbox -> test -> replay -> evaluate ->
   canary -> stable. The agent may grow capabilities, never evaluation or
   permission Core authority.
8. **Evidence-gated later:** smarter non-vector lifecycle policy, ContextCore,
   vector/learned retrieval, and cross-session memory only after baseline
   measurement.

## Definition of done for architectural changes

An architectural change is incomplete unless:

- dependency direction still satisfies this contract;
- output/work is bounded where it crosses a runtime boundary;
- runtime events make the behavior explainable;
- tests cover the new context/tool lifecycle and relevant failure path;
- `docs/ARCHITECTURE.md` or `docs/CONTEXT_LIFECYCLE.md` is updated when the
  contract changes.
