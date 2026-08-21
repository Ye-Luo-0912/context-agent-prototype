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
   main engineering is M12/M13.

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
context-contextcore      -> agent-process (+ agent-contracts)
agent-runtime            -> agent-platform-protocol + agent-core + agent-workspace
agent-tui                -> composition of implementations
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
- The core's turn-stateless primitives (permissions/approval, effect brokering,
  events/audit/durability, budgets, capability/sandbox authority, runtime
  integrity) are the trust boundary of an agent-modification-resistant
  `agent-core`. Evolvable orchestration stays in `agent-runtime`; the fold
  is into `agent-core`, never the orchestrator or a parallel orchestrator.
  **Runtime evolves; Core stays trusted.**

PLAT-01's narrow in-process `CorePort` is the only Core facade held by
`RuntimeServices`: the concrete `CoreAuthority`, its event/approval/effect/
output component handles, and the concrete `RuntimeActor` type are private,
and dependency conformance tests enforce the forbidden production graph.
Explicit capability/plugin admission and state primitives remain public Core
contracts used by their registries. `RuntimeServices` and `spawn_runtime`
remain public trusted-composition seams; their scheduling fields/methods are
private.
Current V1 still runs Core and Runtime as operator-trusted code in one address
space. Core owns the monotonic authority epoch, bounded operation registry and
Core-issued lease/effect identity; Runtime requests epoch advances and remains
the sole scheduler. Production composition persists authority transitions and
reconciles builtin workspace effects before reopening mutation. Stale,
duplicate, corrupt, unmanaged or ambiguous work fails closed behind
`RecoveryRequired`; it is never blindly replayed. Generic shell/process
spawn/exit recovery is landed (identity-safe leftover kill; no rollback of
child mutations; `process.session` recovery is keyed by the start identity). Generic `shell.exec` / `process.run` / `process.session`
require Core-issued effect identity before spawn and return a plain value;
they remain a non-transactional exception, not a prepared-effect commit.
Out-of-process capability/MCP invoke recovery is landed
(durable reserved/dispatch/ack journal; in-flight idempotency keys refuse a
second send; no rollback of peer mutations; not a general HTTP exactly-once
broker). WAL compaction is landed. The semantic query/cancel DTOs,
authorized transport-independent Platform router, WAL-first
`OperationAccepted` publication and RuntimeActor-owned exact-current-tool
control seam are landed; no adapter may bypass the actor to reach Core. Core
serializes exact cancellation against terminalization and persists both its
epoch fence and cancellation truth before Runtime publishes a first-attempt
success after its distinct `TurnCancelled` barrier; Core cancellation truth
alone is never upgraded to a replayed ACK. Partial WAL failure fences both
layers. In-process authenticated operation-control session/grant installation
is landed. Framed JSON-lines operation-control over an inherited-pipe
   analogue (`agent_process::FramedProtocolSession` + the authenticated adapter)
is landed; Named Pipe/UDS remain PLAT-08. `ProcessHost` composes
`ProcessSupervisor` and `DuplexTransport` (stdio first backend); MCP stdio
owns the same supervisor (kill then await reap on poison/timeout/cancel).
`ProcessHost` advertises `host_epoch` on ping; adapters expose
`ConnectionHealth` (Ready/Degraded/NotServing/Quarantined) and a bounded
`RestartCircuit` (PLAT-06 slice 1; first connect is not a restart). Peer
cancel-ACK frames and coalescible bounded progress are landed (PLAT-06
slice 2; settlement is still kill-then-reap). Multiplexing remains later
PLAT-06. Connection state is never task or Core authority.
Linux landlock write fencing is landed; TCP bind/connect is denied on ABI
v4+ (`MOD-07`) when write roots are configured. ABI v5 denies device ioctl
(`MOD-12`). ABI v6 also scopes
outbound signals (`MOD-11`). Windows Low-IL write
confinement is landed (`MOD-08`). Unix `RLIMIT_AS` is landed (`MOD-09`;
capability default 2 GiB VAS). Unix `RLIMIT_FSIZE` is landed (`MOD-10`;
capability default 256 MiB). Unix `RLIMIT_NOFILE` and inherited-fd close
are landed (`MOD-13`; capability default 1024 fds). The Windows integrity
wrap Job-Object now also caps the real child's commit at 512 MiB (`MOD-14`).
Unix `RLIMIT_CORE` is forced to zero when sandbox `pre_exec` runs (`MOD-15`;
not a `0` = unlimited field). Linux `RLIMIT_NICE`/`RLIMIT_RTPRIO` are
clamped to zero and `no_new_privs` is set in that same hook (`MOD-16`).
Windows Job-Objects pin `PRIORITY_CLASS=NORMAL` and leave breakaway
default-deny (`MOD-17`). UDP, raw sockets, pathname Unix, Windows
OS-level network fences, and I/O bandwidth quotas remain. After MOD-17
there is no further allowed v0 sandbox slice; do not invent `MOD-18`
from that residual or from multiplexing / Named Pipe/UDS. Do not claim M13
closed. HANDLE_LIST and Job UI restrictions stay skipped (Command
inheritance is not clean; UI tests are flaky).
Local transport identity is never
itself a Core grant. Parse-time decoded JSON budgets (`JsonDecodeBudget`) are
landed in `agent-platform-protocol` and applied at process, MCP, context-service
and operation-control parse sites; they bound the DOM while visiting so a
frame-legal empty-object array cannot inflate decoded memory. RFC 8785 JCS is
the `ArgumentDigest` canonicalization. Explicit `legacy.invoke-output.v1`
handshake negotiation is landed (plain `ToolOutput` is default-deny). The
shared process/context/MCP adapter fault matrix lives in `agent-conformance`.
Adapter envelope migration onto Platform DTOs remains PLAT-07.
This is not general crash exactly-once or a
non-bypassable boundary against a malicious same-process Runtime; platform-
specific durability limits are documented in `docs/ARCHITECTURE.md`.

Forbidden dependencies:

```text
agent-core  -> agent-runtime | context-simple | agent-tui
agent-runtime -> context-simple | tool-runtime
tool-runtime  -> context-simple | ContextEngine
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

This is the milestone dependency, not the day-to-day work queue; ROADMAP's
code-grounded priority list may interleave containment work such as PLAT-00.
M12 and M13 must still finish before Self-Iteration:

1. **V1-M10 Runtime Consistency:** task authority, transactional transitions,
   checkpoints, and turn commit. Runtime and context must never split-brain.
2. **V1-M11 Context Recall:** store injection, bounded `ContextMapView`,
   search/fetch, `gc_epoch`, and async store. Recall must not pollute prompt
   history.
3. **V1-M12 Effect Runtime:** route every brokerable capability side effect
   through one `EffectRequest`/commit path. Generic shell/process execution
   is an explicit, approved non-transactional exception until its admission
   and recovery contract closes; cancellation limits future work but cannot
   roll back mutations already performed by a child.
   PLAT-03a4 closes builtin workspace reconciliation, and RuntimeCheckpoint v4
   cross-checks a stable Core authority-journal prefix without rewinding it.
   Operation query/cancel DTOs, the authorized transport-independent router,
   WAL-first `OperationAccepted` publication and the actor-owned control seam
   are landed. In-process authenticated operation-control session installation
   is landed. Framed JSON-lines operation-control over an inherited-pipe
   analogue is landed. Out-of-process capability/MCP invoke recovery is
   landed. Artifact locators now carry owner plus an immutable content
   digest (`artifact://v1/<run>/<owner>/<digest>`); live captures use an
   explicit draft form until seal. Parse-time decoded JSON DOM budgets,
   RFC 8785 JCS argument digests, explicit `legacy.invoke-output.v1`
   negotiation and the shared adapter fault matrix are landed. Adapter
   envelope migration onto Platform DTOs remains PLAT-07. A future HTTP/gRPC
   broker must use the
   same reserved/dispatch/ack
   barrier before M12 or V2 can claim generally recoverable remote effects.
4. **V1-M13 Extension Sandbox:** scrub environment; broker filesystem/network;
   enforce cancellation and permissions.
5. **V1-M14 Resource Policy:** bound tool schemas, context hints, `RiskClass`,
   and `PermissionSet` so meta-tools cannot exhaust runtime resources.
6. **V1-M15 Real Evaluation:** coding-workload A/B/C plus lifecycle, cost, and
   latency metrics. Save tokens without reducing task success. Acceptance
   evidence must be reproducible from versioned per-cell artifacts; repeated
   runs of one task do not substitute for independent tasks.
7. **V2 Self-Iteration:** generate -> sandbox -> test -> replay -> evaluate ->
   canary -> stable. The agent may grow capabilities, never evaluation or
   permission Core authority.
8. **Evidence-gated later:** smarter non-vector lifecycle policy, ContextCore,
   vector/learned retrieval, and cross-session memory only after baseline
   measurement. None of these are in evidence; do not start them to chase
   extra rounds.

## Definition of done for architectural changes

An architectural change is incomplete unless:

- dependency direction still satisfies this contract;
- output/work is bounded where it crosses a runtime boundary;
- runtime events make the behavior explainable;
- tests cover the new context/tool lifecycle and relevant failure path;
- `docs/ARCHITECTURE.md` or `docs/CONTEXT_LIFECYCLE.md` is updated when the
  contract changes.
