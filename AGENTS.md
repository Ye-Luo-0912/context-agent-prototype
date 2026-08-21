# Agent Development Contract

This repository is an experimental coding-agent runtime focused on
**continuous context lifecycle management**. Preserve this contract before
changing architecture.

| Doc | Role |
| --- | --- |
| [`docs/STATUS.md`](docs/STATUS.md) | Now / freeze / P0–P1 / next milestone |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Stable architecture |
| [`docs/CONTEXT_LIFECYCLE.md`](docs/CONTEXT_LIFECYCLE.md) | Context / GC / evidence / retrieval |
| [`docs/EXECUTION_COHERENCE.md`](docs/EXECUTION_COHERENCE.md) | Execution Coherence V1 |
| [`docs/PLATFORM_SECURITY.md`](docs/PLATFORM_SECURITY.md) | EffectIntent / sandbox attestation |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestone gates and ordered route |
| [`docs/AUDIT_TODO.md`](docs/AUDIT_TODO.md) | Confirmed defect queue |
| `crates/agent-eval/evidence/*/REPORT.md` | Experiment facts |

Do not duplicate those lists here. Do not treat
`docs/CONTEXT_RUNTIME_TODO.md` as live contract.

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
   authoritative global `messages: Vec<_>` to `CoreAuthority`.
2. **Token pressure does not trigger forgetting.** Maintenance is driven by
   `UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`,
   `TaskCompleted`, and `Checkpoint`; budgeting is final packing only.
3. **Tools cannot access context engines or memory stores.** Tools return
   `ToolOutput`; kernel/context policy decides what enters context.
   `tool-runtime` may depend only on contracts plus narrow workspace/process
   facilities; it must not depend on a protocol host, runtime or context
   implementation.
4. **Raw tool output is not prompt history.** Store large output once under
   `.focus-agent/artifacts/...`; keep `ToolOutput::model_content` bounded.
5. **Context implementations are replaceable.** `agent-core` depends only
   on `ContextEngine`; ContextCore must arrive as an adapter/implementation,
   never a kernel rewrite or a `context-simple` import.
6. **UI is event-driven.** UI code consumes runtime events, not mutable
   kernel/context internals. Evolve `AppState` toward a reusable
   `RunStateAggregator`/view model.
7. **Raw traces are filesystem artifacts.** Use JSONL/artifact files; do not
   add a database merely to store traces or learning data.
8. **Keep v0 non-vector.** No embeddings, vector DB, graph retrieval, or RAG
   until the dynamic working-set baseline is measured. Context operational
   core is a freeze candidate: do not retune GC thresholds or reactivation
   scoring; do not add a learned router. After Execution Coherence V1,
   main engineering is M12/M13. Do not claim those milestones closed.

## Architecture and dependencies

Allowed high-level direction:

```text
agent-contracts
 ^
 +-- agent-platform-protocol (bounded semantic wire DTOs + parse-time JSON budgets; no transport/runtime)
 +-- context-simple
 +-- agent-workspace
 +-- tool-runtime (also -> agent-workspace + narrow agent-process control)
 +-- agent-storage
 +-- agent-process (framed IPC, child lifecycle, sandbox hooks; decode budgets via protocol)
 +-- agent-core

agent-capability-process -> agent-process (+ agent-contracts + agent-platform-protocol)
context-contextcore -> agent-process (+ agent-contracts)
agent-runtime -> agent-platform-protocol + agent-core + agent-workspace
agent-tui -> composition of implementations
```

- `agent-core` is the **turn-stateless Core facade**: contracts, budgets,
  approval, events/audit/durability, and model/tool/context wiring through
  traits. It owns no task, turn, or prompt-frame state; minimal authority
  registries may remain stateful.
- `agent-runtime` is the **only orchestrator**. It owns `RuntimeActor`, task
  and scope lifecycle, prompt assembly, capability registry, module host, and
  adaptive policy. Never put turn state back in the core or add a second
  orchestrator.
- Concrete context engines (`context-simple`, `context-baselines`) and tool
  dispatchers (`tool-runtime`) are wired only by trusted composition roots
  (`agent-compose`, with `agent-tui` as one product host); runtime remains
  implementation-agnostic.
- **Runtime evolves; Core stays trusted.** PLAT-01's narrow in-process
  `CorePort` is the only Core facade held by `RuntimeServices`. Current V1
  still runs Core and Runtime as operator-trusted code in one address space.
- Generic `shell.exec` / `process.run` / `process.session` stay a
  non-transactional exception: Core identity before spawn, kill-then-reap
  on cancel, no rollback of child mutations. Do not invent `MOD-18` from
  residual syscalls or from multiplexing / Named Pipe/UDS. Untrusted
  generated code fails closed unless the host can attest the required
  sandbox floor; WASI is a V2 candidate, not a v0 slice.
- ToolSpec is model-visible schema, not authority. Trusted
  `HostToolPolicy` binds builtin args to `EffectIntent`. A plugin cannot
  self-authorize via `ToolRisk` plus parameter names.
- Process-connection state is `HostLifecycle` (`NeverStarted` / `Serving` /
  `Quarantined` / `Stopped`). First connect is not a restart. A failed
  replacement stays quarantined and must consume `RestartCircuit`.

Forbidden dependencies:

```text
agent-core -> agent-runtime | context-simple | agent-tui
agent-runtime -> context-simple | tool-runtime
tool-runtime -> context-simple | ContextEngine
context-simple -> tool-runtime
agent-contracts -> any concrete crate
agent-platform-protocol -> any crate except agent-contracts
isolated/adapted Platform clients -> agent-core | agent-runtime
```

`agent-conformance` enforces the production/build graph transitively; test-only
composition dependencies do not weaken these shipped-library rules.

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

Foreground evidence packs first and charges **actual** tokens against the
frame budget; Runtime must not worst-case-reserve `MAX_FOREGROUND_TOKENS`.
GC-induced reread is `Warm` + `Stored` only.

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

See [`docs/ROADMAP.md`](docs/ROADMAP.md) and [`docs/STATUS.md`](docs/STATUS.md).
M12 and M13 must still finish before Self-Iteration. Do not claim those
milestones closed. Context live `context-mech.v2` (12 cells) evidence is
under `crates/agent-eval/evidence/context-mech/`; do not retune GC from
it. `add_test` is Tool Surface, not Context.

## Definition of done for architectural changes

An architectural change is incomplete unless:

- dependency direction still satisfies this contract;
- output/work is bounded where it crosses a runtime boundary;
- runtime events make the behavior explainable;
- tests cover the new context/tool lifecycle and relevant failure path;
- `docs/ARCHITECTURE.md`, `docs/CONTEXT_LIFECYCLE.md`,
  `docs/EXECUTION_COHERENCE.md`, or `docs/PLATFORM_SECURITY.md` is updated
  when that contract changes.
