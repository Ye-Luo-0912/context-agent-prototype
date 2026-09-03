# Agent OS Architecture

Stable architecture: authority, runtime, tools, context. Sibling contracts:

- [`STATUS.md`](STATUS.md) — now / freeze / P0
- [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) — operational state
- [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md) — EffectIntent / sandbox
- [`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md) — GC and retrieval
- [`ROADMAP.md`](ROADMAP.md) — milestone gates

## 1. Purpose

This prototype is not intended to become a second ContextCore. It is a small, real agent runtime used to validate the runtime behavior that ContextCore will eventually power.

Primary hypothesis:

> Long-running agents should continuously maintain a bounded, task-focused working set. Completed or low-value information should leave active context during execution, not remain in an append-only transcript until a context-window threshold forces compression.

The first version therefore prioritizes runtime/context boundaries over model features.

## 2. Layering

The diagram combines the **landed PLAT-01 CorePort boundary**, **landed
PLAT-02 semantic protocol contract** and **landed PLAT-03a1-a4 Core authority
plus builtin workspace recovery** with the remaining
recovery/adapter/SDK/supervision work in
`PLAT-03..07`; it is not a
claim that every seam is already a security boundary.

```text
UI / API / user ingress
          │ RuntimeCommand / RuntimeEvent
          v
┌──────────────────────────────────────────────────────────────┐
│ Platform (evolvable user space) — agent-runtime              │
│ RuntimeActor: the only task/turn orchestrator                │
│ TaskAnchor / Episode / Focus / prompt and surface planning   │
│ ModuleHost / target process supervision / extension lifecycle│
└───────────────┬──────────────────────────┬───────────────────┘
                │ CorePort                 │ Platform Protocol
                │ typed internal calls     │ for isolated modules
                v                          v
┌─────────────────────────────┐   ┌────────────────────────────┐
│ Target Trusted Core /       │   │ Tools / Skill workers /    │
│ reference monitor           │   │ Agents / MCP / Context     │
│ grants / leases / effects / │   │ services / model adapters  │
│ hard bounds / audit         │   │ (no direct Core access)    │
└───────────────┬─────────────┘   └────────────────────────────┘
                │ brokered resource operations
                v
┌──────────────────────────────────────────────────────────────┐
│ Workspace / process / network / artifact / durable storage   │
└──────────────────────────────────────────────────────────────┘

agent-compose / agent-tui = bootloader and product composition only;
they do not own a second scheduler or authority loop.
```

**Current V1 deployment.** `RuntimeActor` is the only task/turn orchestrator,
and PLAT-01 now makes its concrete type private. Public trusted composition
constructs `RuntimeServices`, whose fields and scheduling methods are private
and whose only Core facade is `Arc<dyn CorePort>`; the concrete
`CoreAuthority` and its event/approval/effect/output component handles are not
exported. Capability/plugin admission and state primitives remain explicit
public Core contracts used by their registries. Mediated effect commits carry
run/turn/operation/effect/digest/epoch/lease identity. Core owns a
monotonic authority epoch and a bounded resident
operation registry. It validates the epoch, exact operation identity and its
issued lease, assigns prepared-effect IDs, and prevents exact duplicate
dispatch/commit within the retained authority history. A production-dependency
conformance test enforces the forbidden graph and prevents isolated adapters
from importing Core or Runtime internals.

This remains one operator-trusted address space, not process isolation. Core
now owns the authority epoch; Runtime requests compare-and-swap advances
and retains only a scheduling mirror. Core rejects stale tool dispatch both
before and after approval, independently rejects stale effect commit, and
cancellation advances the Core fence and installs an exact-identity terminal
reservation before any await or cleanup. A delayed admission therefore observes
the cancelled duplicate instead of dispatching. The bounded
registry keeps unresolved operations and recent terminal state, exposes an
in-process query, and never treats an evicted known ID as unseen. Its fixed-size
seen-ID filter is fail-closed: a collision is reported as
`ExpiredOrPossiblySeen` and can reject a genuinely new random ID. PLAT-03a3 now
persists epoch and operation transitions through a contracts-only journal
injected by composition. Core performs journal-first state publication;
`FileOperationJournal` uses an exclusive writer, checksummed monotonic records
and `sync_all`, repairs only a structurally incomplete final fragment, and
fails closed on other corruption. It refuses new records before bounded
recovery/file limits could make the next startup unrecoverable, and compaction
preflights the output record count against that recovery limit before writing a
new generation, switching metadata or deleting the old WAL
(`DURABILITY-BARRIER-01`, repaired in `f055e39`). PLAT-03a4 now
preallocates the Core-issued `EffectId` before builtin workspace dispatch and
passes exact operation/digest/effect identity into a separate, bounded,
checksummed, exclusively locked and `sync_all`-barriered workspace-effect
journal. Startup reconciles both journals and current file hashes: proven
not-applied or applied mutations become durable terminal truth; partial,
corrupt, unmanaged or ambiguous effects remain unresolved and raise the Core
`RecoveryRequired` mutation fence. Generic shell/process tools now record
spawn/exit in a sibling authority journal: never-spawned work is
`NotApplied`, a durable wait settles as `CompletedValue`, and a crash
window without exit stays `Ambiguous`. Leftover children are killed only
when the OS create-time token still matches; PID reuse is never killed.
Process recovery cannot roll back mutations the child already performed.
`process.session` recovery is keyed by the **start** identity: stop or a
poll that records exit settles that spawn as `CompletedValue`. Poll/stop
never spawn, so their own identities stay `NotApplied`.
Out-of-process capability/MCP invokes now record reserved/dispatch/ack in
`.focus-agent/authority/remote-effects.jsonl`: never-sent work is
`NotApplied`, a durable Completed/Failed ack settles as `CompletedValue`,
and a dispatched crash window stays `Ambiguous`. An in-flight idempotency
key refuses a second send. This is not a general HTTP exactly-once broker
and cannot roll back peer mutations. RuntimeCheckpoint v4 now cross-checks an ancestor Core
WAL marker before restore. The protocol crate defines strict operation
query/cancel routes and truthful response DTOs. `agent-runtime` now owns a
transport-independent `OperationControlRouter`: it resolves opaque authority
references through a trusted composition-provided authorizer, hides foreign
runs, canonicalizes complete identities from Core query truth, and forwards
read-only queries and exact-current-tool cancellation through the sole actor.
The router owns no Core handle, registry, transport or scheduler.
Core linearizes cancellation against operation terminalization: a won cancel
persists the epoch fence and exact cancellation terminal before Runtime
publishes it, while a terminal/commit state that won first is returned
unchanged and does not spuriously fence the actor. A partial authority-WAL
failure leaves both Core and Runtime behind an observable recovery fence.
Core `CancelledBeforeCommit` truth alone is not a Runtime cancellation ACK:
without a durable actor-side `TurnCancelled` acknowledgement marker, a lost
reply is resolved by query but is not reported as a replayed success.
Tool admission is now split from execution: Core first returns a non-cloneable
admission permit after the `Accepted` snapshot crosses its authority-WAL
barrier. Runtime installs the in-flight operation and hands that permit back
to Core; Core publishes `OperationAccepted`, then `ToolStarted`, and only on
both successes upgrades it to the distinct one-shot dispatch permit.
Authorized live subscribers therefore have a race-free identity source; an
exact duplicate has no permit and cannot redispatch. Event history remains
observability rather than operation authority, so lag is reported and the
caller must query Core instead of guessing a missing identity. Concrete authenticated session/grant installation for operation-control is
landed as an in-process adapter: composition installs a bounded session,
the adapter binds that session to the connection and overwrites peer
`authority_ref`, and query/cancel still go only through `RuntimeActor`.
Framed JSON-lines operation-control over an inherited-pipe analogue is
landed (`FramedProtocolSession` reads/writes bounded frames; the adapter
still consumes one frame body and never owns the pipe). Named Pipe/UDS
remain PLAT-08; parse-time decoded JSON DOM budgets, RFC 8785 JCS, explicit
`legacy.invoke-output.v1` negotiation and the shared adapter fault matrix are
landed (`PLAT-04`). Adapter envelope migration onto Platform DTOs remains
`PLAT-07`. Artifact owner/digest
identity locators are landed. This is not a persistent
local-service endpoint. V1
query/cancel requests target only the logical operation and
must omit `effect_id`; the returned Core snapshot is authoritative for effect
state. WAL compaction is landed (generation fold + bounded exact-tip
ancestors). Generic process spawn/exit recovery is landed. In-process
authenticated operation-control session installation is landed. Out-of-process
capability/MCP invoke recovery is landed. Saturation metrics remain open.
Unix synchronizes newly created parent directories; Windows syncs the file but
retains an explicit power-loss directory-entry limitation because directory
`FlushFileBuffers` is not a supported barrier.
PLAT-03 is therefore partial. The workspace-local recovery slice is closed,
but the system does not claim general crash exactly-once or a non-bypassable
boundary against a malicious or self-modifying same-process Runtime. The common Platform
Protocol/SDK and unified process supervision also remain open.

## 2b. Trust model: four rings, one orchestrator

The system arranges its components into four trust rings. Trust here
means "who can modify what, and at what point in the run"; each ring has a
distinct registration path and a distinct default posture.

1. **Trusted core (`agent-core`)** — turn-stateless, operator-trusted
   authority primitives: `CoreAuthority` (event envelope identity /
   sequence / durability, approval verdict normalization, mediated effect
   commit/rollback, bounded output brokering)
   and the admission/state authorities (capability admission, activation /
   quarantine / maturity, plugin package admission, plugin activation).
   The private concrete `CoreAuthority` owns no task, turn or prompt-frame
   state and is exposed to `RuntimeServices` only through `CorePort`;
   capability/plugin admission and state authorities remain explicit Core
   primitives and intentionally own minimal mutable authority records. PLAT-01
   narrowed the call surface; PLAT-03a1-a4 added the Core-owned epoch, bounded
   operation registry and journal-first persistent transitions used for stale
   and duplicate dispatch/commit validation, plus exact startup reconciliation
   for builtin workspace mutations. Generic shell/process spawn/exit
   evidence is now reconciled the same way, and those tools now fail closed
   without Core-issued effect identity before spawn (they still cannot
   roll back mutations the child already performed). Out-of-process capability/MCP
   invokes now use the same reserved/dispatch/ack journal. A future HTTP
   broker must reuse that barrier; peer mutations are never rolled back.
2. **Platform / runtime orchestrator (`agent-runtime`)** — evolvable
   user-space policy and service hosting:
   `RuntimeActor` owns the turn state machine (turn frame, lifecycle transition
   requests and what to commit), the task manager, scope lifecycle, prompt
   assembly and the local epoch mirror; `RuntimeServices` owns scheduling;
   the module host, capability registry and plugin registry own the extension
   catalogs.
   There is exactly one orchestrator: no other component owns turn state
   or a second command loop.
3. **Trusted Platform composition plane (`agent-compose`, `agent-tui`,
   `tool-runtime` builtins + trusted modules)** —
   operator-trusted wiring. `ModuleHost::add_module` and
   `ServiceRegistry::register` publish typed services (context, model,
   tool, approval, event, artifact) at composition time; a module is
   refused after the host started. Composition adapters are not ordinary
   plugins: they extend the Platform's trusted service plane, never Core
   authority or the model-visible catalog. The composition root is a
   bootloader, not a second orchestrator.
4. **Dynamic extension plane (process capabilities, future executable Skill
   workers, child Agents, MCP adapters, plugin packages)** — runtime-loadable
   and permissioned Platform clients.
   Capabilities register through `register_capability` mid-run; every
   out-of-process transport is pinned to `Experimental` + `Disabled` at
   registration and enters the model surface only after explicit enable.
   Their tools join the dispatcher under the registered grant; Skills and
   hooks are declarative metadata that never execute in v0, so only a Tool,
   Hook or worker referenced by a future Skill package becomes a protocol
   peer. The SDK/protocol dependency is the target of `PLAT-07`, not current
   code.

The rule that binds all four rings: **there is one orchestrator**. The
runtime actor drives every turn; the core stays turn-stateless and never gains
a turn loop; a second orchestrator is never introduced. Dynamic capabilities
reach authority only through Platform, while current operator-trusted
in-process code remains outside a technical isolation boundary.

Vocabulary: *composition module/adapter* names operator-trusted services
on the composition plane; *capability* names runtime-loadable
actions/services on the dynamic plane; *Skill*, *Hook* and *Plugin
Package* are defined separately in the manifest (ECO-01/ECO-03/ECO-06/
ECO-07) — skills and hooks are validated metadata, only tools are
interpreted.

## 2c. Target Agent OS boundary and Platform Protocol

The OS analogy is architectural, not a reason to turn every crate into a
service:

- **Target Core becomes the reference monitor.** It owns only mechanisms that
  must eventually be non-bypassable:
  admission and grant ceilings, short-lived leases, generation validation,
  effect commit/rollback, hard resource bounds, durable audit identity, and
  quarantine. It owns no TaskManager, turn loop, prompt, tool-selection
  policy, child-Agent scheduler, or concrete extension.
- **Platform becomes the syscall gateway and service manager.** The existing
  `RuntimeActor` remains the sole orchestrator. It owns task/context policy,
  lifecycle, unified process supervision, routing and user/model interaction,
  but it cannot mint authority beyond the limits Core records. Today it owns
  extension lifecycle while concrete adapters still own their child handles.
- **Extensions are Platform clients.** Tools, Skills, hooks, child Agents,
  MCP servers and independently deployed context/model adapters request
  Platform services. Isolated processes never receive Rust Core/Runtime
  objects or raw workspace roots. Trusted in-process capabilities may receive
  permission-scoped `WorkspaceHandle` / `ArtifactHandle` views; those are
  least-authority Platform facilities, not concrete unrestricted workspaces.

Two contracts must remain distinct:

1. **`CorePort` (landed in `PLAT-01`)** is the narrow internal authority API
   used by Platform. `RuntimeServices` retains only `Arc<dyn CorePort>` as its
   Core facade; the concrete Core, its event/approval/effect/output components,
   and the concrete actor type are private. Commit requests carry run, turn,
   operation, generation and lease identity. Core validates run identity, its
   issued lease and its independently owned authority epoch; Runtime can only
   advance the epoch through compare-and-swap. Composition injects both the
   persistent authority journal and the builtin workspace reconciler. Core
   reconciles recorded managed mutations and fails closed on ambiguous state;
   the preparation-order/hash limits documented under Mutation transactions
   prevent claiming universal or cryptographic recovery truth. RuntimeCheckpoint v4 now carries a
   stable journal-lineage/generation/prefix/digest marker, validates it before
   restore mutation, and only advances the live epoch; it never embeds or
   rewinds operation truth. Typed operation query/cancel DTOs and the
   authorized transport-independent router, WAL-first acceptance publication
   and actor-owned control seam are landed. In-process authenticated
   operation-control session installation is landed. Framed JSON-lines
   operation-control over an inherited-pipe analogue is landed.
   Out-of-process capability/MCP invoke recovery is landed.
   Artifact owner/digest identity locators are landed (sealed SHA-256;
   live captures remain explicit drafts until seal). Parse-time decoded JSON
   DOM budgets are landed (`JsonDecodeBudget` at process/MCP/context-service/
   operation-control parse sites). PLAT-04 common-contract proof is landed
   (JCS, `legacy.invoke-output.v1` negotiation, shared adapter fault matrix).
   Adapter envelope migration onto Platform DTOs remains `PLAT-07`.
   Named Pipe/UDS remain a later measured transport.
   The port stays an in-process typed Rust call in V1 for cheap, clear
   transactions. If Self-Iteration makes a modifiable Runtime part of the
   threat model, the same port may require authenticated isolation; Core still
   gains no task loop and therefore does not become a second orchestrator.
2. **Platform Protocol semantics (landed in `PLAT-02`)** live in the bottom-
   layer `agent-platform-protocol` crate. They define strict typed UUIDs,
   physical request versus logical operation/effect identity, exact negotiated
   profiles, schema/argument digests, monotonic remaining deadlines, bounded
   one-hop causality, an explicit success/error response carrier and a legal
   retry/effect-state algebra. Core envelope structs reject unknown fields;
   artifact owner/digest locators are landed as typed payload DTOs.
   Parse-time decoded JSON DOM budgets are landed. RFC 8785 JCS and the
   shared adapter fault matrix are landed (`PLAT-04`). Adapter envelope
   migration stays `PLAT-07`.
   The protocol becomes the stable boundary for isolated,
   independently versioned or untrusted components. It owns version/feature
   negotiation, schema identity, operation and effect ids, deadlines,
   cancellation, structured errors, recovery queries, bounded frames and
   artifact references. Tool/effect, context, ingress and managed-Agent
   operations are typed namespaces of this protocol, not all one generic
   `Tool` message. A declarative Skill is package metadata, not itself a peer.
   The existing process/context/MCP adapters do not yet carry this envelope;
   their migration remains `PLAT-07`, while recoverable operation state
   remains `PLAT-03`. Operation-control DTOs now
   exist, but no existing process/context/MCP adapter is permitted to treat
   that as a migrated or authorized wire endpoint.

**One semantic protocol does not require one physical transport.** All wire
backends must carry the same bounded envelope and pass the same conformance
suite, while deployment chooses the least exposed transport:

| Boundary | Default transport |
| --- | --- |
| Trusted, same-process hot path | direct trait / actor message |
| Platform-spawned, one-to-one child | inherited anonymous pipes (dedicated protocol handles; stdout/stderr are logs) |
| Persistent or independently started local service | Windows Named Pipe / Unix Domain Socket, with ACL or peer-credential checks |
| External ecosystem or remote service | MCP, HTTP or gRPC adapter terminating into Platform Protocol |
| Large/binary result | immutable artifact/ref; never the control channel |

Transport identity is connection admission, not operation authority. Every
request is still checked against the registered grant, run/task scope,
short-lived lease, operation generation and effect-specific decision. A local
socket, process id or successful handshake never grants permission by itself.

## 3. Dependency direction

The intended dependency direction is strict. `agent-platform-protocol` is a
landed bottom-layer semantic DTO/validator crate that depends only on
`agent-contracts`; `agent-platform-sdk` remains a planned extraction.

```text
agent-contracts <- agent-platform-protocol (landed semantic DTOs)
      ^                         ^
      │                         └── agent-platform-sdk (planned extension API)
      ├──────── agent-core          (private concrete Core behind landed CorePort)
      ├──────── agent-process       (current host/control; target supervisor + transport + framing)
      ├──────── context-simple / agent-workspace / agent-storage
      ├──────── tool-runtime        (trusted builtins; narrow workspace/process facilities)
      └──────── agent-runtime       (Platform; sole orchestrator)
                       ^
                       ├── trusted in-process adapters
                       ├── process/MCP/context/Agent Platform adapters
                       └── agent-compose / UI / API composition roots

external Tool / Skill / Hook / child Agent
      └── agent-platform-sdk -> agent-platform-protocol
          (forbidden: extension -> agent-core | agent-runtime internals)
```

Important consequences:

- `agent-core` does not import `context-simple`.
- `tool-runtime` does not import `context-simple` or any memory implementation.
- `agent-compose` is the reusable composition root; `agent-tui` is one
  product/frontend root that chooses concrete implementations.
- `agent-runtime` is the only orchestrator and never imports a concrete
  context engine or tool dispatcher; `agent-core` never imports
  `agent-runtime`.
- `agent-conformance/tests/dependency_boundaries.rs` checks the complete
  production layer graph transitively through a layer/role matrix: every
  workspace crate carries exactly one role, a role may depend only on its
  admitted suppliers, and new crates fail until assigned a role. It keeps
  `agent-contracts` at the bottom and prevents isolated/adapted Platform
  clients from depending on Core or Runtime
  (`DEPENDENCY-CONFORMANCE-01`, repaired in `2436249`).
- Target dynamic extensions depend on Platform contracts only. They never import
  `agent-core`, call `CorePort`, access RuntimeActor internals, or turn a
  connection identity into a permission.
- Trusted in-process implementations may satisfy the same semantic contract
  by direct trait call. Process separation follows trust, failure, language
  and independent-upgrade boundaries, not crate count.
- `context-contextcore` implements `ContextEngine` over a process without
  changing the Platform contract; the framed transport it uses is the shared
  `agent-process` host, so the context service and native process capabilities
  share one host implementation. `ProcessHost` composes
  `ProcessSupervisor` and `DuplexTransport` (`PLAT-05`; stdio first
  backend) and reaps after kill on poison/timeout/cancel. MCP stdio owns
  the same `ProcessSupervisor` (JSON-RPC stays MCP's; it does not speak
  the native ping/invoke host protocol). `PLAT-00` contains the current
  wire faults; `PLAT-06` slice 1 health/epochs/restart is landed
  (`ConnectionHealth`, `ConnectionEpoch`, `RestartCircuit`). Peer cancel-ACK
  and coalescible progress are landed. Remaining
  `PLAT-06` (multiplexing) and
  `PLAT-07` SDK-facing adapters remain. Named Pipe/UDS remain `PLAT-08`.

**Implementation audit note (2026-08-31; repaired 2026-09-01).** The shipped
graph still has `agent-workspace -> agent-process` for process journal
identity/kill helpers even though those crates are drawn as siblings. That
edge is now deliberately admitted by the layer/role matrix above
(`2436249`); the earlier hard-coded denylist could not catch it, new role
crates, or the type-level ban on `tool-runtime` using `ContextEngine`, all of
which the matrix enforces.

## 4. Stable contracts

### ContextEngine

The Platform needs a small replaceable context surface; Core must not expose
it to extensions:

1. `ingest` — submit a meaningful runtime observation.
2. `maintain` — trigger continuous lifecycle maintenance (the semantic
   state machine: active/cooling/archived/dropped with reasons).
3. `materialize` — select the model-facing working set as structured items.
4. `gc` — run a full mark/sweep/reactivate pass (the physical compactor:
   root marking, reversible eviction, generations). Has a default no-op
   implementation, so engines without a GC pass (baselines, the wire
   adapter) keep working unchanged.
5. `diagnostics` — expose bounded observability.
6. `search_external` / `inspect_external` / `fetch_external` — the
   deterministic retrieval surface (default no-ops, so engines without a
   store keep working). Search and inspect cover the live catalog
   (Resident/Warm heap projections plus Cold/External store descriptors)
   on indexed dimensions (entity signature, kind, scope, task, label);
   fetch pulls stored content only. See `docs/CONTEXT_LIFECYCLE.md` §9g.

The API is asynchronous even though `context-simple` is in-process. This
leaves room for a future ContextCore service adapter over local IPC/HTTP/gRPC
without changing the Platform's `ContextEngine` contract.

Since V1-P0-2 the engine never renders prompt text. It answers a
`ContextQuery { current_input, budget_tokens, hints }` with a
`MaterializedContext { focus, items, external, selected, foreground, approx_tokens,
diagnostics }`; `items` are structured `MaterializedItem`s, `external` is
a bounded refs-only view (store hot/recency plus Warm items that are hot
or already Checked; max 32 entries, never the full map), and the runtime-owned `PromptAssembler` is the only place that turns
them into `ModelMessage`s.

Three implementations exist behind the same contract, plus the P5
process-boundary adapter:

- `context-simple::SimpleContextEngine` (C) — dynamic working set with
  lifecycle states (active/cooling/archived/dropped), focus affinity,
  scoring, transitions, and a full GC pass (V1-M6) that separates
  residency (resident/evicted), generation and semantic state and evicts
  into a bounded reversible buffer.
- `context-baselines::AppendOnlyEngine` (A) — append-only transcript,
  no maintenance.
- `context-baselines::RollingSummaryEngine` (B) — append, then collapse
  history older than a verbatim recency window into a summary marker once a
  token threshold is crossed.
- `context-contextcore::ContextServiceAdapter` (P5) — the ContextCore
  integration shape: implements `ContextEngine` over a spawned
  `agent-context-service` process speaking a JSON-lines stdio protocol.
  The service runs any in-process engine; swapping it for a real ContextCore
  runtime only changes what process is behind the pipe.

The RuntimeActor remains implementation-agnostic; the composition root selects it
(`agent-tui --context=append|rolling|dynamic|service`). `agent-replay
--compare` replays the same scripted scenarios through all three and reports
token cost and churn (`docs/EXPERIMENTS.md`).

`SimpleContextEngine` (C) also implements the P4 explicit features:
decision supersession and the error lifecycle (persist → recurring
supersession → verified-fixed archive). Both are configurable and emit
explainable `ContextStateTransition`s (`docs/CONTEXT_LIFECYCLE.md` §9b).

### ModelTransport

The model provider is deliberately outside Core. A Platform adapter translates
`ModelRequest`/`ModelOutput` to a vendor API.

Neither Core nor the RuntimeActor depends on provider-specific response IDs,
SDK types, or streaming structures.

The P1 contract adds:

- `capabilities()` — `ModelCapabilities` (streaming, tool calls, max output
  tokens) so the UI/runtime can branch without vendor knowledge.
- `complete_stream(request, sink)` — the RuntimeActor always drives the model through
  this. The provider normalizes vendor wire chunks into `ModelChunk`
  (`TextDelta`, `ToolCallDelta`, `Done`) delivered to a `ModelEventSink`, and
  returns the final assembled `ModelOutput`. A default implementation bridges
  a non-streaming `complete` into a single delta, so every transport works in
  the streaming loop.
- Cancellation — `ModelRequest.cancel` is a `CancellationToken`; the provider's
  stream loop `select!`s on it and aborts with `AgentError::Cancelled`. The
  RuntimeHandle exposes `cancel_current_turn()`, while the actor checks the token between tool rounds,
  and ends a cancelled turn with a durable, typed `TurnCancelled` event.
  `TurnCompleted` remains reserved for a successfully committed model/context
  result, while `RuntimeHandle::cancel_turn` returns `TurnCancelAck` only after
  the cancellation barrier passes.
- Streaming deltas are live-only: `RuntimeEvent::ModelDelta` is broadcast to
  UI subscribers but never journaled — the final `AssistantMessage` carries
  the complete content for replay. Its envelope repeats the durable journal
  cursor of the `ModelStarted` that opened the stream; it does not allocate a
  journal sequence number, so persisted traces remain contiguous. Since V1-M9 each delta carries
  `turn_id`/`operation_id`/`generation` and the UI's `RunStateAggregator`
  accepts deltas only for the operation it currently renders, so a late
  delta from a cancelled turn can never leak into the next turn's view —
  the live stream is fenced the same way the final `OperationResult` is.
- Retry/backoff lives at the transport boundary: `provider-openai` ships a
  generic `RetryingTransport` wrapper. Network, timeout, 5xx, 429 and
  gateway-wrapped upstream failures use the configured bounded exponential
  transport budget. Model-emitted malformed tool arguments use a separate
  format class: at most one immediate regeneration, never the outage backoff.
  A genuine 400, damaged SSE event or output-limit outcome is not retried.

`provider-openai` speaks both Responses and Chat Completions SSE. Protocol is
an explicit adapter setting (`responses`, `chat`, or `auto`); `auto` probes
`<base_url>/responses` once and caches Chat fallback only for an explicit
unsupported-endpoint result. It never changes `base_url` or fails over between
providers. Thus PinAI is contacted directly at its own `/v1/responses` base,
while a localhost OpenCode relay remains a separate provider route. Responses
input is rebuilt from the Runtime-owned model frame (`message`,
`function_call`, `function_call_output`) with `store=false`; no provider
conversation becomes Context authority. Chat `finish_reason=network_error`
and retryable Responses failure events remain transport failures rather than
structurally empty successful turns. Responses `response.incomplete` is
semantic: `max_output_tokens` becomes the non-retryable `ModelOutputLimit`
outcome and other incomplete reasons become model outcomes, not provider
outages. All vendor wire parsing stays in the provider crate. Function names
on either wire are mapped to
`^[a-zA-Z0-9_-]+$` (`.` and `:` become `_`); inbound calls are mapped back to
Core ids. Two Core ids that collapse to the same wire name fail closed before
the HTTP call. Kernel tool ids are unchanged.

A Chat stream is complete only after `[DONE]`; a Responses stream only after
`response.completed`. EOF before that terminal, malformed JSON in a valid
`data:` frame, missing fields on a known Responses event, or incomplete tool
arguments is a typed `ModelProtocol` failure. Unknown well-formed extension
events may be ignored. An unterminated SSE line is capped by the configured
total stream-byte limit before the decoder can grow it further.
`ModelEventSink::creates_replay_barrier` is fail-closed
by default. Runtime's live sink declares tool-call deltas protocol-internal and
text deltas irreversible, so a malformed call can be regenerated before any
text publication but published text is never replayed. Buffering eval sinks
publish only a successful attempt and are bounded by both chunk and byte caps.
`RetryAfterMillis` is bounded and backoff arithmetic is checked or saturating.

### ToolDispatcher

Tools receive a `ToolExecutionRequest` and return `ToolOutput`.

They do not receive a ContextEngine, memory store, or conversation transcript.

`ToolDispatcher` separates pure discovery/snapshot reads, explicit lifecycle
maintenance (`gc`, load/unload), omission classification, inspection and
execution. `snapshot()` returns the complete currently-loaded candidate set;
it does not perform the final schema-budget projection because only the
runtime knows the active task's requirements. A model round publishes exactly
one final `ToolSurfaceSnapshot` and uses it for the budget, prompt and
tool-call validation, so lifecycle changes after capture cannot change what
the model saw.

Since V1-M9 the lifecycle contract is unified for builtin tools and dynamic
capabilities under one catalog:

- `catalog()` — every known tool (builtin + capability) with lifecycle state
  and owner, for `capability.search` / `capability.inspect`;
- `load_tool(name)` / `unload_tool(name)` — host/operator persistent source:
  move exactly one tool on/off the model surface until explicit unload;
  loading one capability tool never surfaces its siblings and never grants
  execution authority;
- `load_tool_for_lease(name)` — Runtime/model lease load. Typed task, active
  call and result-delivery roots keep it visible. A trusted model-explicit load
  also creates a turn-local pending-use root until exact use, unload, or
  directive end, allowing adjacent loads to coexist without a round TTL. A
  source-free optional moves to Warm at the next safe decision boundary.
  Providers without source tracking retain the compatibility fallback to
  `load_tool`;
- `inspect_tool(name)` — one tool's full spec;
- `execution_attribution(call)` — fail-closed, bounded pre-dispatch semantics:
  trusted purpose, canonical resource identities and whether a registered
  Verify operation may be reused within one task anchor. It is not part of
  `ToolSpec`, grants no authority, and cannot be supplied by producer output
  metadata. Dynamic capabilities default to Unattributed; shell/process are
  Opaque;
- `ToolLifecycle::{Available, Loaded, Active, Warm, Unloaded}` is the one
  lifecycle shared by both planes: registered-but-not-loaded capability
  tools are `Available` and do not grow the prompt, the builtin catalog
  cools `Loaded -> Warm -> Unloaded` on idle ticks, and the unified control
  tools (`capability.search/inspect/load/unload`, always visible) drive the
  active set.

#### TaskToolRequirements, typed roots and round surfaces

The runtime owns the sole schema-budget projection over the complete loaded
candidate set:

- each runtime-owned `TaskRecord` carries a bounded, canonical
  `TaskToolRequirementSet { revision, entries }` and replaces the whole set
  through actor-serialized compare-and-swap. `TaskInfo` exposes the current
  revision/count, and live restore rebases against a per-process high-water
  mark so an older checkpoint cannot create a CAS ABA;
- requirements use an exact tool name plus `MustSurface`, `PreferSurface`, or
  `KeepReady`. They express task demand only and cannot enable/quarantine a
  capability or grant approval/effect authority;
- a pure typed-root policy derives additional roots at the BeforeModel safe
  point: the task anchor's structured fields map to explicit tool families
  (acceptance criteria -> verification, open loops -> exploration, plan
  progress -> mutation, working refs -> artifact access), the focus goal
  without a task derives exploration, and the active-call policy pins the
  executing tool as `MustSurface`. Derivation is deterministic,
  de-duplicated, catalog-filtered and bounded, and the explicit task-owned
  set stays the authority;
- an anchor-bound trusted verification-source row may PreferSurface the exact
  verifier that previously produced current evidence. If that schema is not
  available, typed role derivation remains the fallback; source affinity does
  not itself authorize execution or prove equivalence;
- an exact verifier may separately opt into `ExactCurrentWorld` only with a
  SHA-256 host identity digest covering recipe, execution-profile, policy and
  relevant environment revisions. Raw environment material never crosses the
  attribution boundary. Runtime reuses a PASS only when task-owned
  execution state, anchor revision, user-directive revision, workspace
  revision, exact tool, canonical argument digest and that host identity all
  match and verification validity remains Current. The receipt is provenance
  on the existing bounded verification fact, never Context/transcript state;
  a new directive or any uncertain identity executes normally. Generic shell,
  process and dynamic capability metadata cannot opt in;
- the production builtin verifier is `verify.run { recipe_id }`, never an
  arbitrary command wrapper. `VerificationRecipes` is bounded, immutable and
  shared by construction: it derives the dispatcher's concrete argv/cwd/env
  and the composition root's `ExecRecipe` host policy. Unknown ids collapse to
  empty authority. The tool is added to the required round surface only when a
  recipe exists, so empty/non-project workspaces pay no schema cost. General
  project test runners remain `TaskScoped`; only a host-asserted
  source-read-only recipe may use exact reuse. Exact capture hashes recipe
  revision, platform, executable, inherited environment and either a declared
  complete input set or a complete bounded workspace snapshot. Runtime samples
  the trusted identity before and after execution and records an exact PASS only
  when they match. Links/escapes, external-input directives, special files,
  races or incomplete/oversized capture keep the result TaskScoped and execute
  later requests normally;
- after the tool-GC safe point, the runtime refreshes required lifecycle flags
  and `RoundSurfacePlan` projects the complete loaded candidate set. Mandatory
  schemas are retained, preferred schemas degrade deterministically under
  schema/provider budget, and KeepReady remains catalog-ready but prompt-cold;
- the final snapshot carries a runtime `surface_revision`, non-colliding
  builtin / capability / task-requirement / anchor / focus /
  execution-policy source revisions, and bounded omission data.
  `ToolSurfacePlanned` is the schema-free audit report; `ModelStarted` belongs
  after a Ready plan and successful final packing.

Capability lifecycle is per tool: loading one tool of a capability never
surfaces its siblings, while process start/stop stays owner-level. Checkpoint
restore migrates legacy whole-capability flags to per-tool lists but treats
them as mechanical residency, not proof of a persistent source; live
composition-time host sources are unioned back after restore.
Every selected/omitted round row carries per-row provenance
(`TaskRequirement` / `DispatcherRequired` / `CatalogLoadedOptional` /
`Unknown` for legacy rows), so task-authored Prefer is distinguishable from
a catalog-loaded optional fallback. Snapshot consistency is verified: the
builtin registry captures specs and generation under one lock, capability
capture/mutation uses the surface gate, and the composite dispatcher holds
that gate while taking one atomic base snapshot to form a common source cut
without retry; concurrency tests cover catalog mutation during capture.

Since V1-M9 the model can also steer the *context* surface through
read-only meta-tools (`context.tag` / `context.lease`, always loaded with
the core set; `gc_hint` and `collect` are not model-facing). They do no work
themselves: each returns a `ToolOutcome::RuntimeDirective` carrying a typed
`ContextAction` (`Collect` runs the GC pass via `ContextEngine::gc`; the
rest become a `ContextDirective` ingest) — tools still never touch the
  engine or memory stores (invariant 3), and the Platform remains the only
  scheduler of how a directive is applied through Core's authority checks. The model addresses items by the
ids exposed in the materialized context frame (`id=<...>` per item), and
the engine silently ignores directives whose target item is gone.

Two guards keep these directives from becoming a backdoor:

- **commit-time execution**: the directive is executed by the actor when
  the tool result commits (inside the operation's generation fence), not
  at turn finalize — a lease or tag lands before the next model round
  observes it. Finalize only persists observations;
- **permission gate**: producing a `RuntimeDirective` requires the
  `runtime:context-control` permission in the capability manifest. A
  random `weather.lookup` tool cannot return a `GcHint`/`Lease`; the
  dispatcher rewrites such attempts into a denied `Value`.

Hints and leases are bounded, never permanent roots: `keep_alive` is
capped per engine (`max_keep_alive_items`), leases are capped per task in
both count (`max_leased_items_per_task`) and weight
(`max_leased_tokens_per_task`) plus a per-directive turn cap
(`max_lease_turns`). A quota refusal surfaces as an `InvalidRequest` error
from the directive ingest, so the model learns its hint was not granted,
and both protections auto-expire when the owning task completes.


`ToolExecutionRequest` carries a `CancellationToken` (`cancel`), so long-running
work (searches, shell processes) can be aborted cooperatively by the caller.
Tool risk classes (`ReadOnly` / `WorkspaceWrite` / `ProcessExecution`) drive
approval before execution.

### Workspace confinement (Trusted Core boundary)

`agent-workspace` is the one path through which file tools touch the disk,
and since V1-P0-5 it is a confinement boundary, not a path joiner:

- `Workspace::resolve_relative` rejects `..`, absolute paths, drive
  prefixes and root components, then walks the candidate component by
  component from the canonical root: every existing intermediate is
  canonicalized and must remain under the root. Symlinks, junctions and
  reparse points anywhere along the path therefore cannot redirect the
  tail outside the workspace; missing tail components are appended
  lexically so new-file writes still work. (Windows verbatim `\\?\`
  prefixes from `canonicalize` are normalized away so returned paths
  stay display-friendly.)
- Since CORE-07 the *authoritative* reads and mutations do not use that
  path string at all: validation and open are fused into one
  directory-handle-relative descent (`confined_open_read`,
  `confined_parent`, staging and replace), so a link swap between a
  validation pass and the open cannot redirect the operation. Each
  component is opened relative to the already-open parent handle with
  link-following disabled — `openat` with `O_NOFOLLOW`/`O_DIRECTORY` on
  Unix; `NtCreateFile` with a `RootDirectory` handle and
  `FILE_OPEN_REPARSE_POINT`, plus an explicit reparse-tag check after
  every open (any nonzero tag — symlink, junction, mount point, cloud
  placeholder — is rejected), on Windows. `resolve_relative` remains for
  display-only resolution (`fs.list`, `search.grep`, `git.diff` pathspec
  validation).
- `Workspace::resolve_mutation` adds a hard rejection of the runtime
  state directory (`.focus-agent/` — traces, checkpoints, artifacts,
  change journal); mutating tools (`fs.write`, `edit.replace`) resolve
  through it, so ordinary coding tools cannot overwrite runtime state.
  File mutations open only an already-existing parent through the confined
  handle chain; they never create directory topology before effect authority.
  Creating directories requires a future separately authorized/recoverable
  effect rather than being hidden inside a file write.
- Read tools (`fs.list`/`fs.read`/`search.grep`) use `resolve_relative`
  and can still read artifacts; `fs.read`/`edit.replace` read through the
  pinned handle, so their size checks and content reads describe the same
  object.

### Mutation transactions

Since V1-P0-6 every file mutation is a `MutationTransaction`. Existing-file
edit batches enter through `Workspace::begin_existing_mutations`; create or
replace paths use `Workspace::begin_mutation`. Both require the parent
directory to exist; only the final file may be newly created:

```text
resolve + canonical ordered leases → pinned bounded snapshot
→ synced authority v3 Prepared (Core-managed) → exclusive short sibling temp
→ stage sync → review Prepared
→ target-revision and staged-identity/length/SHA checks
→ handle-relative atomic replace → installed-byte checks + authority ack
→ review terminal + final installed-byte check
```

Since CORE-07 the staging and the swap are relative to the pinned parent
directory handle — `renameat` on Unix,
`NtSetInformationFile(FileRenameInformation)` with the parent as
`RootDirectory` on Windows — so neither the staged file nor the final
replace can be redirected by a path swap.

For Core-managed writes the synced authority v3 `Prepared` intent lands
*before the staged entry is created*, so every temp that can survive a crash
already has its deterministic name, target, byte lengths, SHA-256 revisions,
and operation identity recorded. Failure to record intent therefore creates
no temp and cannot mutate the target. The context-free `prepare` entry is only
for trusted tests/maintenance and is explicitly not crash-recoverable. Before
replace, commit reopens the target through the pinned parent and compares its
exact revision, then verifies that the staged open handle still has the
expected bounded length/SHA. Unix also
checks that the sibling name still identifies that handle's device/inode
immediately before its name-based rename. Windows instead creates the staging
handle with shared read only and renames that handle itself, denying competing
shared write/delete access. A pre-replace refusal removes the temp and records
rollback; cleanup or rollback-journal uncertainty is `Unknown`, not a false
`NotApplied`. `fs.write`, `edit.replace`, and `edit.patch` use this path.

Directory topology uses a separate single-component transaction, never an
implicit side effect of a file write:

```text
confine exact path + acquire target lease + pin existing parent
→ authority-v3 Prepared(kind=directory, bytes=0)
→ Core generation/intent fence
→ handle-relative create of the absent final component
→ sync where supported + capture stable filesystem identity
→ authority Committed(identity) + review terminal + final identity check
```

`fs.mkdir` requires the immediate parent to exist. This makes one prepared
effect equal one visible topology transition; callers express a deeper path as
separate approved effects instead of receiving a hidden partially-created
chain. An existing directory is an idempotent value and carries a no-mutation
execution fact. Rollback deletes only the exact pinned directory while it is
still empty and unchanged. If another actor substitutes or populates it, if
cleanup cannot be confirmed, or if a crash occurs after create and before the
committed identity, the result is `Unknown`/`Ambiguous`. Unix identity is
device+inode and directory entries receive a parent sync; Windows identity is
volume serial+file index, competing write/delete sharing is denied while the
effect settles, and the existing documented directory-entry power-loss window
remains because Windows exposes no supported directory flush equivalent.

On Unix the staged file receives the original mode bits before replace. On
Windows the replacement's readonly permission is restored after replace;
this does not preserve the complete ACL, alternate streams, hidden/system
attributes, or timestamps. After a successful replace the runtime validates
the installed entry against the retained open handle and staged length/SHA,
syncs the parent, records the synced authority acknowledgement and flushed
review terminal, then validates again before returning `Durable`. A mismatch
immediately after replace is
`Unknown`; drift before the final acknowledgement is
`Applied + DurabilityFailed`.

Two journals serve different purposes. `.focus-agent/changes.jsonl` is the
serialized review/revert log. Its records are individually bounded and each
complete line is `write_all` + `flush`, but the file is not `sync_all`'d or
size-capped. Its normal transaction shape is:

```text
MutationPrepared { tx_id, target, before_hash, after_hash }
        │
        ├─ atomic rename ok → MutationCommitted { tx_id }
        └─ pre-replace refusal + successful cleanup/log
                                      → MutationRolledBack { tx_id }
```

`.focus-agent/authority/workspace-effects.jsonl` is the distinct authority
journal: exclusive/pinned, framed and SHA-256-checksummed, `sync_all`'d, and
bounded. V3 keeps the v2 file record fields (deterministic staged name,
operation evidence, byte lengths and SHA-256 revisions) and adds an entry kind
plus committed stable identity for directories. Real v1 FNV-1a-64 and v2 file
frames remain checksum/read compatible; every file version uses 4 MiB-per-file
and aggregate reconciliation read bounds. Directory recovery performs no file
content read and succeeds as Applied only when the current object identity
matches the committed identity.

Reconciliation opens target and stage through confined handles. It deletes a
stage only after proving regular-file type, complete expected bytes, and that
the name still identifies the opened object before and after hashing; missing
stage after a pre-allocation intent is safely `NotApplied`, while partial,
substituted, colliding, or otherwise unprovable content is retained and
reported `Ambiguous`. Authority `Committed` precedes review `Committed`, so a
crash between them can still leave review history at `Prepared`; the authority
journal, not the review log, governs recovery truth. `Effect::rollback`
returns `AgentResult<()>`: success confirms owned
preparations were cleaned and their required rollback terminals landed;
cleanup/journal failure is bounded `RecoveryRequired`. Composite and staged
rollback try every child in reverse order, Core installs its mutation recovery
fence on any unconfirmed settlement, and Runtime distinguishes
`not_applied_cleanup_recovery_required` from
`execution_cleanup_recovery_required` rather than emitting a plain rejection
or preserving proposed revisions as applied facts.

Commit results are structured, because "the file did not change" and
"the file changed but I could not record it" need different recovery:
`EffectReceipt::NotApplied` (rename never landed — target intact),
`EffectReceipt::Applied { durability: Durable | DurabilityFailed }`, and
`EffectReceipt::Unknown` (terminal applied/cleanup state cannot be proved). A swap that
lands but whose `MutationCommitted` record cannot be appended is
`Applied + DurabilityFailed`; Runtime requires recovery and never reports
"no change" to the model. The swap itself goes through the pinned-parent
`replace_file` primitive — Unix `renameat`, or Windows
`NtSetInformationFile(FileRenameInformation)` on the staging handle with
`ReplaceIfExists` and the parent as `RootDirectory`. It never uses a
remove-then-rename sequence that would break per-file replacement atomicity.


### Tool lifecycle (V1-P6)

The builtin dispatcher is a catalog with an active set, not a fixed list:

```text
ToolCatalog → capability discovery → ActiveToolSet → model request
```

Tools move through `Available → Loaded → Active → Warm → Unloaded`; only
loaded/active tools appear in `specs()`. `specs()` is pure: the actor runs
`ToolDispatcher::gc()` once per model round at an explicit safe point
(start of the round, before the budget), so budget, prompt assembly and
tool-call validation all observe one stable surface per round — a tool
cannot age out from under the model between seeing its schema and calling
it. The always-visible control
tools `capability.search` / `capability.load` / `capability.unload` let
the model discover and drive the lifecycle (load `git.status` when the
task needs git). Execution of a catalog tool is always permitted — the
lifecycle gates the model surface, and the approval gate protects side
effects.

### Typed labels (V1-P5)

`ContextItem.tags` is `Vec<Label>`, never raw strings: `CoreLabel`
(decision/finding/constraint/open-loop/artifact-ref/evidence-ref),
`LifecycleLabel` (promoted/superseded/verified-fixed) and
`Label::Extension` for namespaced module labels (`ext:github/pr`).
Labels serialize as their string form and accept any string back, so old
checkpoints and future labels round-trip; a misspelled tag can no longer
silently change promotion or GC behavior.

### Approval flow

`agent-core` owns the `ApprovalGate` contract. Three implementations
exist:

- `PolicyApprovalGate` — automatic policy (`read_only()`, `permissive()`, or a
  custom mix). No UI involved.
- `InteractiveApprovalGate` — used by the TUI. Works with an `ApprovalBroker`
  (shared hub): Core broadcasts an `ApprovalRequest` (request id +
  tool call + spec) and waits on a oneshot; the UI subscribes to the broker,
  shows the tool name and a bounded args preview, and answers y/n/Enter/Esc
  through `InteractiveApprovalGate::respond`. Late subscribers can drain
  `broker.pending()`.
- `TaskApprovalGate` — task-scoped standing grants wrapping any inner gate.
  A grant binds one `ToolRisk` effect to a target (workspace path prefix,
  process command prefix) with a bounded constraint (`max_content_bytes`,
  `max_runs`) and an expiry. The model can use a matching grant without a
  per-call prompt but can never create, widen or extend one: grants are
  established by the composition root (`agent-tui` parses `--grant=<json>`),
  listed via `/grants`, revoked via `revoke`, and an expired or exhausted
  grant silently falls through to the inner gate. Matching is component-aware
  on paths and lexical on command tokens, so `src/../outside/x` or
  `cargo testx` never borrows a `src/` / `cargo test` grant. A granted call
  resolves with zero interaction; an ungranted call falls through, so a
  missing responder denies without privilege expansion.

Read-only tools always auto-allow. The wait for an answer is bounded
(default 5 minutes, configurable): if nobody responds, the request is denied
and the pending queue is cleaned, so a missing responder can never hang a
turn. `agent-tui` runs in interactive mode by default; `--read-only` selects
`PolicyApprovalGate::read_only()` (and rejects `--grant`).

```text
CoreAuthority.execute_tool
   │  authorize(call, spec)
   ▼
InteractiveApprovalGate ── broadcast ──► ApprovalBroker ──► TUI prompt (y/n)
   │  (oneshot wait, bounded)                                  │ respond(id, decision)
   └────────────────────────◄──────────────────────────────────┘
   ▼
ToolDispatcher.execute(ToolExecutionRequest { cancel, .. })
   ▼
OutputBroker.bound(run, output)   ── cap fields / spill oversized to artifact
```

### Output broker

A trusted `OutputBroker` (`agent-contracts`) runs inside Core before
any `ToolOutcome` reaches the actor. Every model-facing field has a hard
cap (`summary` 2 000 chars, `model_content` 16 000 chars, serialized
`metadata` 8 000 bytes, decoded total 24 000 chars); oversized
`model_content` spills to the run's artifact directory once
(`agent-workspace::WorkspaceOutputBroker`, composition-root injected by
`agent-tui`) and the preview keeps both ends plus an `artifact://`
reference, so a producer that did not spill no longer loses the truncated
middle. The same broker bounds `context.fetch` results after the engine
answers, provider/model error text is capped before it enters the event
stream, and `context.search` limits are clamped in execution
(`CONTEXT_SEARCH_MAX_LIMIT`), not only in the JSON schema. The actor's
last-line guard (`agent-runtime::output`) stays as a second, cheaper
defense for producers that bypass the broker.

### EventJournal

Runtime events form the learning/replay substrate. The initial implementation is an append-only JSONL journal. This records what the runtime actually did without turning governance/report data into the online hot path.
`UserMessageAccepted` is a bounded preview plus identity (`CTX-EVENT-03`);
the exact user body is stored once as a `user-input` artifact when a
workspace is wired. Old journals that stored full `content` still
deserialize. A second dialogue while a turn runs occupies one in-memory
`Queued` slot; overflow is `Rejected`. `/cancel` still uses the durable
`TurnCancelled` barrier and then emits `InterruptCommitted`. Replay
resolves `body_ref` when given a workspace (`--workspace`).

A `RuntimeCommitBarrier` described as durable is acknowledged only after the
event file (and required directory metadata) reaches stable storage.
`FileEventJournal::flush` syncs each trace file — and a brand-new file's
directory entry — before acknowledging (`DURABILITY-BARRIER-01`, repaired in
`f055e39`), so the acknowledgment now satisfies the crash/power-loss
contract rather than only process-visible ordering.

## 5. Agent loop

```text
User input
   │
   ├─ create/activate long-lived Task; replace current TurnIntent
   ├─ seed ActiveTurn.execution from TaskRecord.resume
   ├─ persist exact body (user-input artifact, if workspace wired)
   ├─ ContextEngine.ingest(UserMessage)   ── full body
   ├─ emit UserMessageAccepted            ── bounded preview + envelope
   ├─ maintain(UserInput)
   │
   v
BeforeModel safe point
   ├─ close settled tool scopes / reconcile pending tool leases
   ├─ revalidate bounded pending path identities
   ├─ derive one RoundExecutionSnapshot + TaskProgressView
   └─ maintain(BeforeModel)
   │
   v
ContextEngine.materialize(ContextQuery)   ── the Context Frame (long-term working set)
   │
   v
PromptAssembler.assemble_with_catalog(runtime_focus, task_anchor, history, turn, tools, catalog)
   = System Policy + Runtime Facts + Tool Catalog Index + Focus Frame
     + Context Frame + Turn Frame + compacted Tool Schemas
   │
   v
ModelTransport.complete_stream()
   │
   ├─ final answer ──────────────────────┐
   │                                     │
   └─ tool calls                         │
       │                                 │
       ├─ trusted pre-dispatch attribution / authority lease
       ├─ approval                       │
       ├─ ToolDispatcher.execute or prepare effect
       ├─ generation fence + commit / rollback
       ├─ artifact raw output            │
       ├─ settle the complete requested batch
       ├─ update ActiveTurn.execution / obligations / frontier
       ├─ bounded ToolOutput ──▶ Turn Frame (execution stack, runtime-owned)
       └─ not ingested during the turn   │
             │                           │
             └──── next model ───────────┘

Final answer
   ├─ persist turn: ingest(ToolObservation) x N + maintain(AfterTool)
   ├─ ingest(AssistantMessage)
   ├─ maintain(AfterModel)
   ├─ GC / input Consumed+Archived
   ├─ durable TurnCompleted barrier
   ├─ install ActiveTurn.execution as TaskRecord.resume
   └─ optional explicit task.complete safe-point transaction
```

The tool result loop is the runtime's execution stack, not long-term memory:
results ride in the `TurnFrame` and never touch the context engine until the
turn ends, when they are persisted as observations and observed by one
`maintain(AfterTool)` pass.

This is the landed ordinary-turn loop. Full `RuntimeCheckpoint` capture and
restore are landed, but automatic checkpoint scheduling and autonomous
continuation from a safe point *inside* one long user directive are not. The
planned extension reuses `TaskAnchor + TaskRecord.resume` and never serializes
the raw transcript; see
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) and the continuation
boundary in [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md).

### The model budget: the engine only sees its slice

Since V1-P0-7 the budget handed to `ContextQuery::budget_tokens` is the
residual of a full request budget computed at the runtime top level
(`agent-runtime::budget::ModelBudget`):

```text
Send window (ModelCapabilities.context_window, else the kernel pack budget)
        - Output Reserve        (max_output_tokens, or a default reserve)
        = Input budget          (runtime final send guard)

Pack window = min(kernel context_budget_tokens, send window)
        - Output Reserve
        - System Policy         (the assembled system prompt)
        - Turn Frame            (wire-form estimate of the turn stack)
        - Active Tool Schemas   (wire-form estimate of the tool specs)
        = Context Frame Budget  (the only number the engine receives)
```

The engine packs `MaterializedContext.foreground` first, charges actual
body tokens, and uses the remainder for the historical working set.
Runtime must not worst-case-reserve `MAX_FOREGROUND_TOKENS` from this
budget; that constant is the foreground cap, not a frame reservation.

The engine never sees the send window, the output reserve or the tool schemas —
it just knows it has N tokens for the working set. A large declared provider
window must not inflate C's working set; append-only A may ignore the pack
query and grow until the send guard trims it. The current user input is
charged inside the turn frame (it rides there), so the engine does not
deduct it a second time. CURRENT FOCUS and TaskAnchor are runtime-owned
and charged at assemble time; the engine's budget is historical working
set only. Pinned items
get selection priority (they go first) but not exemption: every selected
item must fit the remaining budget, so the frame is a hard bound.

Two refinements since V1-M9 close the accounting gap:

- **Dependency expansion is a hard cap.** The reserved expansion slice was
  the last place a pinned dependency could exceed its budget; the pinned
  exemption is gone — expansion spends only the reserved slice, for every
  item.
- **The runtime is the final referee, against the input budget.** `ContextEngine`
  budgets are targets, not verdicts. After `PromptAssembler` renders the
  full five-layer request (system policy, focus frame, context frame, turn
  frame, tool schemas — including the `SELECTED WORKING CONTEXT` /
  `CURRENT FOCUS` rendering overhead, which the engine never accounts),
  the runtime estimates the wire tokens and trims the context frame
  (largest unpinned item first) until the assembled request fits
  `send_window - output_reserve` — the *input* budget. Rendering
  overhead may never eat into the space reserved for the answer. When the
  context frame is emptied and the fixed layers (system + turn + tools)
  still overshoot, the round planner omits eligible optional schemas from
  that request without changing their loaded lifecycle state. Missing or
  over-budget mandatory tools make the plan unsatisfiable; a request that
  still does not fit is a **hard error** — the runtime refuses to send
  instead of silently over-budgeting the provider.

Materialization is now a **non-consuming preview**. Each preview carries a
monotonic `materialization_id`; it may advance internal clocks/revisions, but
it does not stamp access or claim model use. The actor caps the preview at
256 full items, runs `PromptAssembler` and the final provider guard, and then
binds the surviving full-item ids plus at most 32 external-ref ids to the
turn, model round and `OperationId` in a bounded `ContextConsumptionAck`.

Only a non-stale successful `ModelOutput` commits that acknowledgement.
Refused, failed, cancelled and stale operations commit none. Core treats
reinforcement plus the bounded `ContextConsumed` audit event as one context
transaction: it checkpoints first and restores if either the engine mutation
or event append fails; the actor aborts the turn rather than committing an
unaudited output. `context-simple` validates that every id belongs to the
referenced preview and still has exactly one residency owner, then stamps
Resident, Warm or External metadata without changing semantic state or
reactivating a body. This closes the false-reinforcement part of CTX-07;
candidate cost, external-ref token accounting and fit-before-top-K remain.

## 6. Context is rebuilt, not replayed

The model-facing request is intentionally rebuilt from state instead of
replaying an append-only transcript. The input is assembled in five layers:

```text
System Policy    - standing instructions (runtime-owned)
Runtime Facts    - bounded host/workspace profile (runtime-owned)
Tool Catalog Index - names of tools not on this round's schema surface
Focus Frame      - current TaskAnchor + Focus from TaskManager (runtime-owned)
Context Frame    - historical working set from MaterializedItem's
Turn Frame       - the current turn's execution stack (runtime-owned)
Active Tool Schemas - compacted tool definitions for this request
```

Role authority follows the same split. Only the system policy and the
focus frame render with the `System` role; the context frame (retrieved
history and external refs) renders as delimited, low-authority `user`
messages and tool results stay `Tool`-role messages, so content retrieved
from files, tools or the store can never gain system precedence over the
operator's instructions (prompt injection defense, `CORE-05`).

The split is deliberate. The context engine owns the long-term working set
and knows nothing about the execution protocol; the runtime owns the turn
stack and never scores or evicts it while the turn is open. Since V1-P0-2
prompt rendering lives in one place only: `PromptAssembler` takes runtime
Focus/TaskAnchor plus the engine's historical `MaterializedContext`. The
engine could not format a prompt even if it wanted to — it never sees the
system prompt or the tool schemas.

#### Turn Frame wire checkpointing (the Protocol Working Set)

The turn's own protocol history is a working set too, not an append-only
log: the wire view keeps only the last `TURN_FRAME_KEEP_EXCHANGES`
completed tool exchanges and replaces older whole call+result groups with
one bounded deterministic `TURN CHECKPOINT` note (no LLM summary). For
long turns the note also carries a receipt index of at most six distinct
persistable outcomes, each at most 96 characters: tool name, `ok`/`failed`,
and the tool-owned short summary. Arguments, raw/model content, artifacts,
and transient context-retrieval results are excluded. The receipt is a
low-authority record that an outcome happened, not evidence that it remains
current; typed TaskProgress/Execution Frontier freshness still decides that.
The runtime's full `TurnFrame` is never mutated — audit, events, and turn-end
persistence still see every step; only the model-facing wire projection is
bounded. `ModelStarted.turn_checkpoint` records only compacted/receipt counts
for evaluation, never receipt text. This changes no ContextEngine selection,
GC, residency, reactivation, or budget rule. Details and the progress/stall
machinery that motivate it are in
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md).

When a selected `fs.read`/file-observation body has an exact
`path@revision` match in the current TaskProgress Fresh set, its low-authority
context header co-locates `workspace_identity=current`. This is a currentness
fact, not an instruction and not permission to omit the only body. It avoids
requiring the model to join distant prompt layers while preserving the rule
that TaskProgress identity alone never replaces file content.

#### Runtime Facts layer (`TOOL-ENV-01`)

`PromptAssembler` places a bounded system-owned facts block immediately after
System Policy. Composition captures the immutable host profile at startup
(normalized OS product/release and architecture); the actor refreshes workspace
markers after a committed durable mutation and after a successful `shell.exec`
or `process.run`. Caps: 1 KiB UTF-8, 16 sorted markers, 64 bytes per marker.
Facts never enter `ContextEngine`, transcript storage or lifecycle scoring, and
they do not repeat the tool catalog. Stale trusted facts are worse than none.

#### Tool Catalog Index

`PromptAssembler` places a bounded `tool_catalog/v1` system block after
Runtime Facts. It lists catalog tools that are **not** in this round's
tools array (name + one-line summary + lifecycle). Caps: 24 rows, 64
chars per summary, 1536 chars total. The model loads by exact name
(`capability.manage` `op=load`). Full JSON schemas stay off the surface
until loaded. Round-surface schemas are compacted: descriptions truncate
to 96 chars and nested JSON-schema `description` keys are stripped.
`capability.manage inspect` still returns the producer spec. Search ranks
by token overlap plus a whole-phrase bonus so `"patch edit file"` hits
`edit.patch`; it no longer requires every token *and* the concatenated
phrase as a substring. Search rows are compact one-liners
(`state<TAB>name<TAB>purpose`, purpose = first sentence of the
description clipped to 96 chars), so one search answers "what does this
tool do" and the model chains straight to load/invoke without a
follow-up `inspect` per candidate.

`shell.exec` binds one dialect for the whole run and names it in the schema.
Windows prefers detected/pinned PowerShell 7, then Windows PowerShell 5.1,
then `cmd.exe`; Unix uses POSIX `sh`. Fallbacks are explicit. `process.run`
remains the no-shell argv path.

#### Model-visible workspace and failure contract

Ordinary `fs.list` / `fs.read` / search / code-navigation calls hide
`.focus-agent` and raw `.git` internals. Sealed evidence is addressed through
`artifact.read` and VCS through `git.*`. Missing paths return a bounded
parent/topology hint without inventing manifests.

`fs.read` starts model content with a JSON-quoted path, the SHA-256 revision
of the exact raw bytes, and `line_ending = none | lf | crlf | mixed`; a mixed
window also carries a bounded `C/L/N` physical-EOL token map. Its displayed
body remains a numbered logical view capped at 400 lines. The full UTF-8 read
and revision lookup share the 4 MiB mutation ceiling and use a `MAX + 1`
bounded growth probe, so every admitted canonical edit target can first yield
a revision without creating an unbounded ingest path. `edit.patch` is the
single canonical mutation visible to the model: root `files[]`, with required
`path`, a `base_revision` copied from `fs.read`, and nonempty `hunks` per entry
(at most 16 files and 64 hunks total). Each model-visible hunk declares
`op = replace | insert_before | insert_after` plus a unique exact `old` anchor
with enough unchanged context. Insert operations preserve the anchor and
insert the explicit `new` bytes beside it; they do not infer separators or
newlines. Omitted `op` defaults to replace only in the compatibility parser,
and ordinal `occurrence` remains parser-only because repeated-text positions
are brittle after earlier hunks. The bounded success echo preserves both the beginning and end of a
large changed span and marks the omitted middle. The runtime still parses the
old top-level single-file shortcut for compatibility, but does not advertise
it; a top-level revision cannot be silently spread across multiple files. `edit.replace` remains
catalog-only and also accepts a revision. Refusals distinguish
`stale_revision`, `no_exact_match` and `ambiguous_match` and return the
current revision plus at most three candidate regions.

Production `edit.patch` validates that `base_revision` equals the transaction
snapshot SHA-256; it does not retain a per-run read ledger proving that the
string came from the latest `fs.read`. The v3 Tool Surface gate verifies that
provenance from the trace as evaluation evidence. The compatibility parser
also still accepts the legacy top-level form, a missing revision and an
omitted operation; this is a wire-compatibility boundary, not a second
model-visible schema.

The matching contract is **newline-token exact**, never fuzzy. LF and CRLF
are the only two physical encodings admitted for one logical newline token;
lone CR and every other byte (including spaces, indentation, case and Unicode)
must match exactly. Uniform targets keep their global style. For a mixed
target, a canonical LF view authorizes only the logical exact raw span, then
`new` newline j inherits physical style j from that span; extras inherit its
last style or a deterministic local neighbor. Multiple logical occurrences
remain ambiguous unless `occurrence` / `replace_all` is explicit. Ordinary
occurrence matching streams positions in constant auxiliary memory; the
mixed-EOL canonical view is bounded by the 4 MiB file cap, replace-all maps
offsets monotonically without collecting matches, output length is computed
before allocation, and both the tool and `agent-workspace` enforce the same
4 MiB result ceiling.

`Workspace::begin_existing_mutations` resolves and rejects duplicate target
aliases, sorts canonical path keys, acquires the whole batch of in-process
leases in that one order, and only then reads one pinned, bounded snapshot per
file. The same scan supplies the bytes transformed by `edit.replace` /
`edit.patch`, the SHA-256 revision, recovery hash, and bounded old-content
capture; the transaction retains the shared lease group through final
commit, rollback, or drop. Thus clones of the same `Workspace` writing one path queue and
re-snapshot the settled winner, reverse-order batches cannot deadlock, and
unrelated paths remain parallel. A prepared mutation still re-hashes the
current target immediately before replace; drift from writers outside this
in-process lease (direct or authority-bypassing filesystem access) settles as
`NotApplied` with `stale_revision` when visible. A second official
`Workspace::open` on the same root is refused by the exclusive authority-log
lock. Hash→rename is still not an atomic filesystem CAS against bypassing
writers. Each replace remains
per-file atomic. A multi-file effect still commits sequentially — it is not a
cross-file transaction — and a later conflict/failure truthfully reports that
recovery is required after any earlier application. Change-journal appends
are serialized per shared `Workspace`. Once the short append lock is held,
one bounded synchronous `write_all(record + newline)` has no async suspension
point, so task cancellation and parallel different-path transactions cannot
splice partial JSON records together.

Both edit success paths carry the new revision plus a bounded after-edit
echo: a line-numbered window of the changed region in the *updated* bytes
(±3 context lines, 120-char per-line clip). Before that optional preview,
`edit.patch` emits a complete `index:revision` manifest in submitted-file
order; at most 16 SHA-256 values keep it bounded and no later revision can be
lost to preview truncation. One hard 1200-character cap applies to the
combined multi-file echo, including its truncation marker, so adding files
cannot multiply the preview bound. This can remove a confirm `fs.read` round
while remaining transient observation text — superseded by the next
same-path echo and compacted with the exchange, never a residency commitment.

Trusted tool results are projected at the Core output boundary. Producers may
pass a typed class as a top-level hint; they cannot author `metadata._runtime`.
The broker strips reserved keys (`_runtime`, `failure_class`, `recovery_hint`,
`retryable`) and writes:

```text
metadata._runtime.failure_class
metadata._runtime.recovery_hint
```

plus a model-visible header on `model_content`:

```text
runtime_failure:
class=command_unavailable
hint=...
```

`MissingProjectMarker` requires a specific command+subcommand, stderr/stdout
evidence that names the marker, and a true confined absence. `rustc` / `pytest`
/ `pip` / `npx` never imply a missing manifest. Failed execution results stay
on the TurnFrame and do not emit `WorkingSetSignal`. The runtime does not
blindly translate or retry. Evaluation counts these classes separately while
keeping every started cell in end-to-end ITT, rounds, latency and cost.

A `MaterializedContext` is a disposable projection of the Context Frame. It
is not the source of truth and should never be persisted as the memory
model.

Since V1-M2 the engine keeps a **scope tree** (Session -> Task -> Focus ->
Tool) as the first-class unit of residency, and since V1-P0-3 membership is
authoritative: every item carries the `scope_id` of the scope it was
produced in. Scopes carry their own lifecycle (Open / Active / Suspended /
Closed) and own the items created while they were active: closing a task
scope promotes its durable outcomes (decisions, findings, constraints, open
loops, artifact/evidence refs, pinned items) to the session and evicts the
rest of the working set. Tool scopes are execution frames driven by the
runtime actor: opened at tool start (`ContextEngine::open_scope`), and
currently closed when preparation of the next model round treats the result
as schedulably consumed (`ContextEngine::close_scope`). The exact content
acknowledgement now distinguishes that scope heuristic from content actually
sent to a successful provider round, but scope-root release is not yet driven
by the acknowledgement; the observation persisted at turn end stays tagged
with the tool scope id.
The scope tree is engine state, exposed through `ContextDiagnostics` counts
and round-tripped by checkpoints; the runtime drives the timing-sensitive
tool frames explicitly, while session/task/focus open from the focus-state
ingests (`Start` / `set_focus`) and task close still flows through
`maintain(TaskCompleted)` so promotion transitions stay observable.

## 7. Tool output policy

`ToolOutput` separates:

- `summary`: short runtime/UI description;
- `model_content`: bounded content allowed into the next model turn;
- `artifact_ref`: raw/large output stored outside the prompt.

Example:

```text
shell.exec
  raw stdout/stderr -> .focus-agent/artifacts/<run>/shell-*.log
  model_content      -> bounded tail + artifact reference
  ContextEngine      -> ephemeral ToolObservation
```

After the model consumes the observation, `context-simple` can drop it during `AfterModel` maintenance.

### P2 tool set

`tool-runtime` registers a bounded builtin catalog behind the `Tool` trait
(`execute(run_id, call_id, arguments, effect_context, cancel)`). The
round surface is the smaller always-loaded set described in §9c; catalog
tools load on demand. Every tool defines an explicit model-content budget
and artifact policy:

| Tool | Risk | Model sees | Raw output |
| --- | --- | --- | --- |
| `fs.list` / `fs.read` / `fs.write` | read / write | bounded listing/content | file |
| `artifact.read` | read | bounded artifact page | immutable artifact |
| `search.grep` | read | ≤ 100 hits (`file:line: content`) | artifact when more |
| `edit.patch` | write | result + revisions + one globally bounded changed-region echo | change journal |
| `edit.replace` | write | result + revision + bounded changed-region echo | change journal |
| `git.status` / `git.diff` | read | ≤ 12 K chars tail | artifact when truncated |
| `shell.exec` / `process.run` / `process.session` | process | bounded ring tail / session page | artifact for non-empty capture (incremental append) |

Model-visible spill continuation is centralized on `artifact.read`.
`process.run` and host-owned `verify.run` do not publish an `artifact_ref` for
a zero-byte stdout/stderr capture; the terminal result says that no output was
produced. Any non-empty or truncated capture retains the sealed artifact and
the same bounded continuation path.
`fs.list`, `search.grep`, and `code.symbols` expose only their bounded
first-page inputs; an overflow result carries the run-owned `artifact_ref` and
next line. Their older snapshot-cursor arguments remain parser-only
compatibility for trusted callers. This prevents a model from inventing an
opaque capability merely because every ordinary first-page schema advertises
one, while preserving bounded immutable-artifact paging.

`search.grep` skips `.git`, `.focus-agent`, `target`, `node_modules`, `vendor`,
`dist`, `build`, `.idea`, `.vscode` and caps files scanned (5000) and bytes per
file (2 MB). It checks the request `CancellationToken` between files and every
256 lines inside a file; a cancelled scan returns `ok: false` with
`metadata.cancelled` and any hits already found (not `Err(Cancelled)`, which
Core would strip to an empty tool error).

`shell.exec` streams stdout/stderr through two reader tasks into a
bounded channel (512), kills the child on timeout/cancel, and appends the full
log incrementally to an artifact via `Workspace::create_artifact`.

### Workspace change journal

Mutating tools record a `WorkspaceChange`
(tool/path/action/byte sizes/old UTF-8 content when ≤ 256 KiB) to
`.focus-agent/changes.jsonl` via `Workspace::record_change`. The journal is the
review substrate; only entries with captured old content are directly
revertible from this log. It is separate from the durable authority journal
and does not put raw file content into context.

## 8. UI model

The TUI does not render internal Core or RuntimeActor objects directly.

```text
RuntimeEvent
    │
    v
AppState / RunStateAggregator
    │
    ├─ messages
    ├─ runtime status
    ├─ tool status
    ├─ ContextDiagnostics
    ├─ selected items (from ContextPrepared)
    ├─ lifecycle transitions (from ContextMaintained)
    └─ pending approval prompt (from ApprovalBroker)
    │
    v
TUI
```

The `context inspect` panel (Tab) is event-driven: it renders the latest
`ContextPrepared` selections and the recent `ContextMaintained` transitions,
so context behavior stays observable without binding widgets to Core, RuntimeActor, or
context internals. Write/process tool calls replace the input line with an
`Approval Required` prompt (tool name + args preview); `y`/`Enter` allow,
`n`/`Esc` deny.

As the runtime grows, `AppState` should become a dedicated `RunStateAggregator` library so other UIs can consume the same stable view model.

## 8b. Replay and checkpoints (P0.5)

`agent-replay` is an offline analysis binary: it reads a JSONL trace from
`agent-storage` and re-runs the recorded ingest/maintain/materialize calls
against a fresh `SimpleContextEngine`, then prints a per-item lifecycle report.

```text
JSONL trace ──> agent-replay ──> SimpleContextEngine (replay)
                  │
                  └──> per-item lifecycle report
                       (entered / consumed by turns / left + why / final state)
```

Checkpoints are the counterpart: `ContextEngine::checkpoint` exports runtime
state (items, focus, counters) to `.focus-agent/checkpoints/*.json`, separate
from the event journal. `restore` reloads it. Traces are for learning/replay;
checkpoints are for durable runtime state.

Since the actor owns the task table, a checkpoint that only covered the
context engine is no longer a complete actor/context snapshot: the runtime's checkpoint is
a `RuntimeCheckpoint` (versioned) wrapping the task manager (task rows +
current task id), the context checkpoint, capability activation state and
store generation/refs, and `RuntimeInstance::restore` puts the actor, context
and capability planes — task table included — back together. RuntimeCheckpoint
v4 also carries a stable marker for one verified prefix of the Core operation
WAL (`journal_id + generation + epoch + sequence + folded-state digest`). It
also records the event cursor reflected by the frozen planes and a
serde-defaulted `terminal_commit` bit. The wire version stays v4: older v4
payloads default both additions to conservative non-terminal values, while
validation rejects a terminal snapshot unless it has no active task and owns
a matching completed-task record.
Restore verifies that marker as an ancestor before any mutation, then advances
the live epoch; checkpoint data can never replace or rewind Core authority.
Ephemeral checkpoints without a marker are same-run only. V4 persists each
task's complete `TaskAnchor` plus `TaskToolRequirementSet`, the runtime focus
revision and last allocated surface revision, but never a derived per-round
`ToolSurfaceSnapshot`; the next safe point reconstructs it. Older versions
deserialize only far enough to receive an explicit unsupported-version error—
there is no silent empty-authority migration.
The `TaskManager` applies its
transitions transactionally: it validates and *prepares* a transition, the
external side (Runtime's Context transaction) commits first, and only then does
the manager commit — a failed `set_focus` never leaves the task table changed.

Focus and clear are multi-step context transactions: Runtime takes a
portable engine checkpoint before ingest + maintenance and restores it on
either failure; the actor commits task authority only after that succeeds,
then publishes audit/UI events. Terminal completion uses the stronger form of
the same rule: Runtime prepares the post-completion Context plane while
retaining its rollback checkpoint, freezes that exact plane together with a
prospective completed `TaskManager` and next focus in one atomic
`RuntimeCheckpoint`. In a composition with a checkpoint store, only a durable
acknowledgement authorizes the in-memory task/focus assignments. A store-less
composition may complete in memory behind an explicit warning, but makes no
resumability claim. Assembly or write failure restores Context and leaves the
task active. After acknowledgement the assignments are infallible;
`TaskCompleted`, the maintenance report and an explicit
`RuntimeCommitBarrier(TaskCompletion)` are flushed as one bounded event batch.
If that audit batch fails, the terminal checkpoint remains the crash-window
truth and ordinary mutation is recovery-fenced rather than rolling a completed
task backwards. Restore validates the redundant active-task fields and restored
engine focus. `RuntimeInstance` installs task/context state
behind a recovery fence, applies capability activation through a fail-closed
meet, and clears the fence only after durable bounded `RuntimeRestored`.
Unknown ids do not count as applied and old Enabled cannot lift a newer
Disabled/Quarantined state. If context rollback itself fails, the actor fences
further mutation and emits `RecoveryRequired` until a known-good full restore
succeeds. For focus/task transitions, an audit-event failure after aligned
state commits is handled the same way: state stays aligned, but the missing
record is an explicit recovery gap rather than a retryable "nothing happened"
result. Cross-plane capture uses a bounded capability-generation handshake; a
moving surface retries instead of returning a mixed snapshot. `CORE-03` owns
the fault regressions for both directions.

Checkpoint-triggered Context maintenance is governed by the same ownership:
Runtime chooses and commits the schedule; Core supplies authority/durability
operations only. Runtime owns the turn-start transaction (ingest + maintenance
+ audit rows) and checkpoint maintenance through its own schedule, fences the
turn when a Context rollback fails, and validates checkpoint restores on a
scratch state before committing (`RUNTIME-CONTEXT-COMMIT-01`, repaired in
`9ba85d3`/`f42a898`/`f622cf3`; the M10 fault-gate re-audit was recorded
2026-09-03 on `e357bed`, record in [`AUDIT_TODO.md`](AUDIT_TODO.md)).

## 8c. Runtime actor and module host (V1-M3, hardened V1-P0-1/V1-P0-4)

Since V1-M3 the runtime is an actor (`agent-runtime`), not `Mutex`
orchestration. Callers hold a cloneable `RuntimeHandle`:

```text
RuntimeHandle ── mpsc<RuntimeCommand> ──▶ RuntimeActor (owns mutable state)
                     │                        │
                     │                        └── model/tool operations
                     │                            └─▶ OperationResult
                     └──── events ◀── broadcast channel (Core event authority)
```

- every mutation (user message, focus, pin, task completion, checkpoint)
  is a command; the actor serializes them, so focus/pin/task commands can
  no longer interleave with an in-flight turn;
- since V1-M9 the runtime owns a `TaskManager`: tasks are long-lived
  execution entities with their own lifecycle
  (`create_task` / `activate_task` / `suspend_task` / `complete_task`),
  and focus is only the *current attention inside* a task —
  `set_focus(task_id, focus)` never mints a task. Re-focusing a previous
  goal resumes the same task (`/focus A → /focus B → /focus A` is
  Task A → Task B → Task A, not three fresh tasks), which is what scope
  suspension/resume and the GC can rely on;
- since V1-P0-1 the actor *is* the runtime: it owns the turn execution
  state machine and the `TurnFrame`. Model rounds and tool calls are
  spawned operations that report an `OperationResult` (run/turn/task/
  scope/operation ids + generation); the actor validates its epoch mirror and
  Core independently validates its owned epoch before dispatch/effect commit;
  only then does the actor commit (turn-frame push, context ingest/maintenance,
  events). Stale results are dropped before they can change runtime
  state — `is_stale` no longer protects only "after the whole kernel
  turn ran";
- since V1-M9, tool side effects are two-phase. Tool *computation* and
  tool *side-effect commit* are separate: `ToolOutcome` is either a
  plain `Value(ToolOutput)` or a `PreparedEffect { output, effect }`
  where the effect is a staged, rollback-able mutation (today:
  `agent-workspace`'s `PreparedMutation` with its journal
  transaction). The actor checks its epoch mirror *between* the two phases and
  Core independently checks its authority epoch before
  effect commit — a stale tool operation requests rollback instead of commit,
  so a prepared external side effect cannot deliberately cross the fence that
  protects model state. Rollback returns a typed result: success means cleanup
  and the required terminal journals were confirmed; failure fences Core,
  publishes bounded recovery-required diagnostics, and cannot be mislabeled
  as a successful rejection;
- `CoreAuthority` is now a private, turn-stateless authority implementation
  behind the landed `CorePort`. Context/tool references needed to implement
  that port remain inside Core, while scheduling lives in `RuntimeServices`.
  The concrete actor and scheduling fields/methods are private; the public
  `RuntimeServices` constructors and `spawn_runtime` remain trusted
  composition seams. Core owns the atomic authority epoch;
  Runtime requests CAS advances and keeps the scheduling mirror. Core has no
  turn loop, turn locks or `TurnFrame`;
- since V1-M9, a tool result can carry a context directive: the actor
  executes the `RuntimeDirective` at operation-commit time, inside the
  same epoch fence that guards effect commit — `Collect` runs
  `ContextEngine::gc()` immediately and emits `RuntimeEvent::ContextGc`,
  everything else becomes a `ContextDirective` ingest, so a hint/lease/tag
  lands before the observation it targets (see the meta-tools under §4
  ToolDispatcher);
- every completed tool result passes a runtime-owned 16,000-character
  model-content guard before it can enter TurnFrame, context or events.
  Normal producers still spill the full result to an artifact; the guard is
  defense against a capability/adapter violating that contract;
- the actor selects on both the command channel and the operation
  completion channel, so `/cancel` is processed mid-operation. Cancellation
  first advances the Core-owned epoch
  before any await or cleanup, then cancels the operation and records it behind
  a distinct durable `TurnCancelled` barrier. Recovery never mistakes that
  audit fact for a successful `TurnCompleted` commit. Tool-scope closure has
  bounded per-scope and total deadlines; a timeout raises `RecoveryRequired`
  instead of acknowledging a cancellation whose cleanup is uncertain. A
  cancelled tool operation remains an explicit pending-cleanup root, so normal
  mutation waits until its late completion has taken the stale-effect rollback
  path.

Platform composition uses a module host over typed services:

```text
ModuleHost ── add_module (register + validate) ──▶ ServiceRegistry (typed lookup)
   │  ContextModule / ModelModule / ToolModule / ApprovalModule /
   │  EventModule / ArtifactModule
   └── start transactional, stop: capabilities first, then modules reverse
```

There is no universal `handle_event`: trusted modules publish typed services
(`ContextService`, `ModelProvider`, `ToolProvider`, `ApprovalPolicy`,
`EventStore`, `ArtifactStore` — all `CapabilityProvider` markers in
`agent-contracts`) and Platform consumers look them up by type. The shared
`agent-compose` bootloader builds the host, resolves one `RuntimeServices`,
constructs one private Core implementation behind `CorePort` and spawns the
sole RuntimeActor. Product frontends and any evaluator claiming product
equivalence use that root and only select implementations. Isolated mechanism
harnesses may construct a deliberately narrowed `RuntimeServices` directly,
but must label that boundary and cannot use it as product-equivalence evidence.

Since V1-M9 the host lifecycle is transactional. Start is all-or-nothing
in order (a failing module rolls back the already-started ones), and stop
is dependency-safe and best-effort: dynamic capabilities are stopped
*first* — a capability that depends on a typed service must die before
the service it uses — then the typed modules in reverse order, and every
stop error is aggregated into one result instead of aborting at the
first failure. `RuntimeInstance` already aggregates Runtime/Host/Actor
layers; the host applies the same rule inside its own plane.

Since V1-P0-4 the host is an extension platform with two planes:

- **Trusted Platform composition plane (typed).**
  `ServiceRegistry::register` / `get` let operator-trusted adapters publish
  and retrieve typed services with their own `CapabilityId`s. This registry
  is not Core, is not exposed over Platform Protocol and is not an extension
  permission path.
- **Dynamic capability plane.** A `Capability` (manifest + runtime object)
  advertises tool schemas that join the runtime's tool provider:
  `CapabilityRegistry` accepts registrations at composition time or mid-run
  (an LLM can publish new capabilities while the runtime runs), and
  `CapabilityAwareDispatcher` merges built-in and capability tools behind
  one `ToolDispatcher`, routing calls by tool name. The manifest declares
  id/version/name/summary/status/provides/permissions/requires/lifecycle/
  transport; requirements are validated at registration, lifecycle is
  honored (eager starts with the host, lazy on first use), permissions
  stay declarative while the advertised tools carry `ToolRisk` levels the
  approval gate enforces, and `CapabilityTransport::Builtin` is the only
  in-process plane.

Since V1-P7 the maturity ladder is a registry rule, not a declaration:
every out-of-process registration is pinned to `Experimental` (the LLM
cannot promote its own module), while trusted in-process capabilities
keep their declared status. The registry exposes the effective status and
a catalog snapshot for the discovery surface.

Since the capability-ownership split, admission is a **core** decision:
`agent-core`'s `CapabilityAdmission` authority owns the registration caps
(schema size, name/description length, per-capability tool count), the
lock-free static validation (id shape, tool schemas, authority
derivation — risk is derived from declared permissions, never
self-declared), the collision pass (duplicate id, missing `requires`,
reserved/owned tool names) against a registry-built `AdmissionContext`,
and the `initial_status`/`initial_activation` decisions. The runtime's
`CapabilityRegistry` delegates to it, so the same admission rules apply no
matter which registry (or future host) asks. The registry keeps only the
mutable surface state (loaded tools, run lifecycle).

The effective **state** is a core record too: `agent-core`'s
`CapabilityStateAuthority` is the single source of truth for each
registered capability's maturity, activation and effective permission
grant. Every read and every transition (enable/disable/quarantine) routes
through it, and checkpoint snapshot/restore round-trips the state through
it; the registry reacts to state changes with the surface effects
(loaded-tool clearing, generation bumps). Registry readers pre-fetch the
authority's state map instead of nesting locks, and all surface writers
stay serialized by the registry's `surface_gate`, so the split adds no
lock-order hazard. The grant is captured at registration, and the unified
dispatcher builds every invocation context from that registered grant —
never from the live capability object — so a capability that returns a
different manifest after registration cannot escalate what it holds.

Since V1-P8 the manifest speaks `provides: Vec<CapabilityKind>`
(tool/skill/service) and `requires` (the old `dependencies` name still
deserializes). External extensions are out-of-process over a framed
protocol; Rust plugins are deliberately out of scope — the ABI is not a
stable plugin boundary, and a crashed plugin must not take the runtime
down.

```text
ModuleHost
   ├─ typed plane   : add_module -> ServiceRegistry (public register/get)
   └─ dynamic plane : register_capability -> CapabilityRegistry (mid-run ok)
                           │
                     CapabilityAwareDispatcher (base tools + capability tools)
                           │
                      RuntimeServices -> round surface -> model -> invoke
```

Capability invocation returns a `CapabilityOutcome`, not a raw `ToolOutput`:
`Value(ToolOutput)`, `EffectRequest { output, effect }` (a staged,
rollback-able mutation Core commits under its authority-epoch fence), or
`RuntimeDirective { output, directive }` (the context-control path, gated on
the `runtime:context-control` manifest permission). The dispatcher maps these
onto Core's `ToolOutcome`, so trusted in-process capabilities can share
the builtin prepared-effect fence.

The process transport currently fails closed on non-empty `WireEffect`s:
`ProcessCapabilityAdapter` accepts a plain `ToolOutput` or an envelope with an
empty effect list as `CapabilityOutcome::Value`, but rejects a declared
mutation before staging until the wire carries typed actual-intent identity
that Core can prove is within the invocation lease. This deliberately disables
the older staging path instead of trusting a broad `workspace:write` word.
PLAT-03/04 must bind `operation_id + effect_id + argument_digest` and actual
intent before structured process effects can be re-enabled. A child that
mutates *outside* the wire contract (direct
filesystem writes, network sockets) is not rollback-safe by construction:
the mid-invoke system broker confines the brokered surface
(permission-gated `fs.read`, deny-by-default network), but direct OS
syscalls remain the child's own until the M13 residual (OS-level
filtering) closes.

Since V1-P0-8 the composition root composes into one `RuntimeInstance`
that owns the `ModuleHost`, the `RuntimeHandle` and the actor `JoinHandle`.
Shutdown is a single ordered step with aggregated errors:

```text
runtime.shutdown()
  cancel any turn
    → boundedly drain a cancelled tool's late completion
      (rollback any PreparedEffect; timeout → RecoveryRequired)
    → stop the actor (Core stop: flush journal, emit RunCompleted)
    → stop the module host (dynamic capabilities first, then
      typed modules reverse — each step best-effort, errors aggregated)
    → join the actor task
    → aggregate errors
```

The actor itself never returns silently: `Stop` replies with the Core
stop result, and the "all handles dropped" path (`rx.recv() -> None`)
runs the same teardown so journal flush and `RunCompleted` do not depend
on the caller remembering to stop. Before Core stop, the actor boundedly drains
the completion of a cancelled in-flight tool; a late `PreparedEffect` is routed
through stale rollback rather than being discarded with the completion
channel. If that explicit cleanup does not arrive by the shutdown deadline,
shutdown reports `RecoveryRequired`.

## 9. ContextCore migration path

The adapter pattern is implemented (P5):

```text
                 ContextEngine trait
                    /          \
                   /            \
        SimpleContextEngine   ContextServiceAdapter
            (prototype)       (process boundary)
                                  │
                          agent-context-service
                          (today: an in-process engine;
                           later: a real ContextCore runtime)
```

The adapter translates the trait's own types across a JSON-lines stdio
protocol (see `context-contextcore::wire`):

```text
ContextIngress      -> request op `ingest`
ContextQuery        -> request op `materialize`
MaterializedContext <- response payload
ContextConsumptionAck -> request op `acknowledge_consumption`
Diagnostics         <- response payload
```

Since V1-P8 the wire protocol is the reference plugin IPC shape: a
versioned handshake (`PROTOCOL_VERSION` echoed on every response, so a
newer or older service is never misparsed), a per-request deadline
(`ContextServiceConfig::request_timeout`) so a wedged service cannot hang
a turn, and a frame-size bound (`max_frame_bytes`) so a broken service
cannot grow the adapter's memory. A future real ContextCore runtime only
has to speak the same protocol — nothing on the agent side changes.

The wire protocol carries only `agent-contracts` types — no ContextCore
vocabulary leaks through the Agent API. A real ContextCore runtime replaces
`agent-context-service` behind the same protocol; Core, tools,
approvals, TUI and provider are untouched (`agent-tui --context=service`).

Do not move Agent Kernel, tools, approvals, TUI, or provider code into ContextCore merely because ContextCore supplies context selection.

## 9b. Process capability boundary: sandbox + cancellation (V1-M9)

M12/M13 first cuts, HostToolPolicy, HostLifecycle, and the per-OS
attestation matrix live in [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md);
both closed 2026-08-27 on clean-tree closure-audit evidence (see
[`STATUS.md`](STATUS.md) and [`ROADMAP.md`](ROADMAP.md)). Do not reopen them
from this section without new authority evidence.

A process capability is not sandboxed by *telling* the child what it may
do — the manifest's `permissions` array is informational, not
enforcement. Since V1-M9 every out-of-process child runs inside an
explicit `ProcessSandbox` (shared `ProcessHost`, since the crate split in
`agent-process::host`, consumed by `agent-capability-process`):

- **Environment whitelist** — `env_whitelist: Option<Vec<String>>`: when
  set, the child inherits *only* the listed parent variables, plus the
  explicit `ProcessHostConfig::env` grants. The process-capability
  adapter's strict profile whitelists `PATH`/`SystemRoot`/`SystemDrive`/
  `TEMP`/`TMP` only, so `OPENAI_API_KEY`, `HOME`, credentials and friends
  never cross the boundary by default. `None` keeps the historical
  inherit-everything behavior (the context-service default).
- **Private cwd** — `cwd: Option<PathBuf>`: the child runs in its own
  unpredictable directory, created at connect, never the parent's cwd. This
  limits accidental/relative-path access; it is not `chroot` or a mount
  namespace and cannot block absolute paths.
- **Process/job limits and CPU quota** — Unix `pre_exec` rlimits
  (`RLIMIT_CPU`, `RLIMIT_NPROC`, `RLIMIT_AS`, `RLIMIT_FSIZE`,
  `RLIMIT_NOFILE`) applied
  right after fork, in the same closure as landlock (`MOD-09`/`MOD-10`/
  `MOD-13`); that hook also forces `RLIMIT_CORE=0` (`MOD-15`; not a
  `0` = unlimited field) and on Linux clamps `RLIMIT_NICE`/`RLIMIT_RTPRIO`
  plus `no_new_privs` (`MOD-16`). Inherited fds other than stdio are closed after landlock.
  The adapter sets 60 s CPU, 16 processes, 2 GiB virtual address space,
  256 MiB per file, and 1024 open files. Windows Job-Object
  `JOB_OBJECT_LIMIT_PROCESS_MEMORY` remains the commit-charge counterpart.
- **OS-level write fence (Linux)** — `landlock_write_roots:
  Vec<PathBuf>` (V1-M13/MOD-06): when non-empty, the child applies a
  landlock confinement in `pre_exec` — no_new_privs via `prctl`, a
  ruleset whose handled access is tried newest-first across ABIs, one
  path-beneath rule per root (opened as `O_PATH` fds in the parent), then
  an irrevocable `landlock_restrict_self`. A claim covering every
  create/modify/truncate/destroy operation requires Landlock ABI v3 or newer;
  the enforced operations are inherited by every descendant. A kernel without
  landlock degrades to a warning, and a child that cannot
  be confined fails the spawn (never runs unconfined). The capability
  adapter confines children to their private dir; stdio MCP servers are
  confined to their private temp cwd. Reads stay unhandled (loader must
  remain readable) and are gated by the app-level broker.
- **OS-level TCP fence (Linux)** — the same ruleset handles TCP
  bind/connect on Landlock ABI v4+ (`MOD-07`, kernel 6.7+). No port rules
  are added, so every TCP port is denied. ABI v6 also scopes abstract Unix
  sockets and outbound signals (`MOD-11`, `LANDLOCK_SCOPE_SIGNAL`) when
  the kernel accepts that attr. ABI v5 handles device ioctl without
  granting it (`MOD-12`), so newly opened character/block devices cannot
  ioctl. UDP, raw sockets and pathname
  Unix stay unhandled. Windows has no Landlock.
- **OS-level write fence (Windows)** — `integrity_write_roots:
  Vec<PathBuf>` (`MOD-08`): when non-empty, the parent labels each root
  Low and re-spawns through this executable as a wrap that drops to Low IL
  before CreateProcess of the real program. Low IL cannot write up to
  Medium objects outside those roots. Reads and TCP stay unhandled. The
  wrap's Job-Object also caps the real child's commit at 512 MiB (`MOD-14`)
  and dies on unhandled exceptions, and pins `PRIORITY_CLASS=NORMAL`
  (`MOD-17`).
- **Unix address-space ceiling** — `max_memory_bytes` (`MOD-09`): when
  non-zero, the child `setrlimit(RLIMIT_AS)` in the same `pre_exec` as
  landlock. Capability children default to 2 GiB VAS; stdio MCP servers
  get the same memory cap. This counts mappings, not Windows-style commit.
- **Unix file-size ceiling** — `max_file_bytes` (`MOD-10`): when non-zero,
  the child `setrlimit(RLIMIT_FSIZE)` in that same `pre_exec`. Capability
  and stdio MCP children default to 256 MiB per file. Not I/O bandwidth;
  Windows has no Job-Object equivalent.
- **Unix open-file ceiling** — `max_open_files` (`MOD-13`): when non-zero,
  the child `setrlimit(RLIMIT_NOFILE)` in that same `pre_exec`, then
  closes inherited fds other than stdin/stdout/stderr (bounded scan).
  Capability and stdio MCP children default to 1024 fds. Not I/O
  bandwidth.
- **Unix core-dump disable** — `RLIMIT_CORE=0` (`MOD-15`): whenever
  `apply_unix_rlimits` runs, both soft and hard core-file limits are
  forced to zero so a crash cannot dump sandbox secrets. Other rlimit
  fields keep `0` = unlimited; there is no `max_core_bytes` field.
  Probe via `getrlimit`, not by crashing.
- **Linux priority freeze (`MOD-16`)** — whenever `apply_unix_rlimits`
  runs on Linux, `RLIMIT_NICE` and `RLIMIT_RTPRIO` are forced to zero and
  `PR_SET_NO_NEW_PRIVS` is set (via `syscall(SYS_prctl)`) so a parent with
  a raised nice/rtprio ceiling cannot leak into the child and a setuid
  exec cannot escalate even when landlock is skipped. Not fields; same
  always-zero meaning as CORE. Probe via `getrlimit` / `PR_GET_NO_NEW_PRIVS`.
- **Windows Job priority pin (`MOD-17`)** — ProcessHost sandbox jobs and
  the integrity wrap job set `JOB_OBJECT_LIMIT_PRIORITY_CLASS` to
  `NORMAL_PRIORITY_CLASS` so the child cannot raise HIGH/REALTIME.
  `BREAKAWAY_OK` / `SILENT_BREAKAWAY_OK` stay unset. Not a rate limit and
  not UI. Probe via `QueryInformationJobObject`.
- **Kill tree on cancel** — `call_with_cancel(op, cancel)` selects the
  framed request against the invocation's `CancellationToken`: on cancel
  (user `/cancel`, superseded operation) it poisons the connection and
  kills the whole process tree immediately — Unix: SIGKILL to the process
  group the child was spawned into (`process_group(0)`); Windows:
  `taskkill /PID <pid> /T /F`. This stops future child work; it cannot undo
  a mutation the child completed before cancellation.
- **Bounded stderr** — `stderr_capture_bytes`: when set (the capability
  profile uses 64 KiB), the child's stderr is piped and drained by a task
  into a bounded ring kept on the host; `ProcessHost::stderr_tail()`
  surfaces the newest bytes for diagnostics. A chatty child can no longer
  inherit unbounded output into the parent console.
- **Bounded control protocol** — the child speaks framed JSON-lines
  (`ping`/`invoke`) with deadlines and connection poisoning, and every
  boundary shares one bounded codec (`agent-process::frame`): outbound
  frames are capped *before* a byte is written, the in-flight cap is
  enforced before each append while reading, and oversize/partial/malformed/
  version/id/envelope faults poison the connection and kill the child tree.
  The codec never treats OS read chunking as protocol state: multiple frames
  delivered together remain distinct; session ids reject unsolicited/stale
  responses. `ProcessHost` adds a per-call cumulative byte budget and a
  control-plane answer cap; the context service and MCP client read and
  write with the same codec and fail closed the same way (oversized broker
  answers degrade to a refusal frame; MCP notifications are flood-capped).
  After a frame is read, `JsonDecodeBudget` bounds the decoded DOM (depth,
  nodes, strings, array/object width) so a frame-legal empty-object array
  cannot inflate memory; this is independent of JCS or adapter envelope
  migration.
  The adapter's system
  broker gates brokered filesystem reads through an allocation-bounded
  primitive (at most a 256 KiB prefix with truncation metadata) and denies
  network by default. Linux landlock additionally fences writes and, on ABI
  v4+, TCP bind/connect, on ABI v5+ device ioctl, and on ABI v6 outbound
  signals; Windows Low IL
  fences writes outside labeled roots.
  Unix `RLIMIT_AS` caps virtual maps, `RLIMIT_FSIZE` caps per-file size,
  and `RLIMIT_NOFILE` caps open fds (inherited fds other than stdio are
  closed after landlock). Sandbox `pre_exec` also forces `RLIMIT_CORE=0`
  (`MOD-15`) and on Linux clamps NICE/RTPRIO plus `no_new_privs`
  (`MOD-16`). The Windows integrity wrap Job-Object caps the
  real child's commit at 512 MiB (`MOD-14`) and pins NORMAL priority
  (`MOD-17`).
  UDP/raw/pathname-Unix, Windows OS
  sockets, and I/O bandwidth quotas remain
  the child's. See §2c: the current stdio is
  an inherited anonymous-pipe backend, not the permanent protocol identity.

`ProcessCapabilityAdapter::from_manifest` applies this hardened profile to
every process capability; `invoke` forwards the call and the granted
permissions to the child. The host enforces env/cwd/rlimit/message
boundaries, and the adapter's mid-invoke *system broker* enforces the
permission-specific access: a child `{"system": "fs.read", ...}` request is
answered only when the invocation holds `workspace:read`, only through the
confined workspace handle, and only for relative, non-escaping paths;
network system ops (`net.fetch`, `net.connect`, `http.get`, `http.request`)
are refused by design — no network permission word exists. Without a broker
installed, any system frame fails closed (poison + kill), and a per-call
cap (`MAX_SYSTEM_REQUESTS_PER_CALL`) bounds how many system frames one call
may issue. Since the
trust-boundary hardening, the manifest
itself is no longer trusted either: the id must pass a conservative
grammar (`validate_capability_id`, lowercase/digit start, `[a-z0-9._-]`,
<= 64) before it is embedded in the working directory or any route, and
the working directory is private and unpredictable
(`context-agent-capability-<id>-<uuid>`) so no two runs share a path and
a hostile pre-created directory cannot be predicted. The broker confines
the *brokered* surface; since MOD-06 the OS-level *write* half of the
direct-OS gap is closed on Linux (landlock fence above — a hostile child
is refused by the kernel when it creates, modifies or destroys state
outside its roots), since MOD-07 TCP bind/connect is denied on Linux
ABI v4+ (no port rules), ABI v5 denies device ioctl (`MOD-12`), ABI v6 scopes outbound signals (`MOD-11`), and since MOD-08 Windows Low-IL write
confinement refuses writes outside labeled roots. Unix `RLIMIT_AS`
(`MOD-09`) caps virtual address space and Unix `RLIMIT_FSIZE` (`MOD-10`)
caps per-file size. Unix `RLIMIT_NOFILE` (`MOD-13`) caps open fds and
closes inherited descriptors other than stdio. Unix `RLIMIT_CORE`
(`MOD-15`) is forced to zero when that `pre_exec` runs; Linux also
clamps `RLIMIT_NICE`/`RLIMIT_RTPRIO` and sets `no_new_privs` (`MOD-16`).
The Windows integrity wrap Job-Object caps the real child's commit at
512 MiB (`MOD-14`) and pins `PRIORITY_CLASS=NORMAL` (`MOD-17`). UDP, raw, pathname Unix,
arbitrary absolute reads, Windows OS-level network fences, and I/O
bandwidth quotas remain open M13
acceptance requirements; after MOD-17 do not invent `MOD-18` from that
residual. Until then **V2 autonomous
capability generation stays gated** — a generated capability only runs
after explicit `enable`, and only inside the sandbox above.

The same "declarations are not enforcement" rule holds on the in-process
side (V1-M14 PermissionSet): `CapabilityAwareDispatcher::invocation_context`
builds handles only from the declared `manifest.permissions` — a
capability that never declared a workspace permission receives no
workspace handle at all, a `workspace:read`-only capability gets a
`ReadOnlyWorkspace` whose write/staged-write paths are refused with an
error naming the missing grant, and unknown permission strings are now
refused at registration (`is_known_permission`), so undeclared access is
denied by construction before any handle could exist. A `workspace:write`
capability gets a `StagedOnlyWorkspace`: the direct `write` path is
refused ("must be staged") and only `prepare_write` works, returning an
`Effect` Core commits behind its authority-epoch fence — a capability
mutation can no longer land during `invoke` (the CORE-01 bypass). Risk is
derived, never self-declared: a capability declaring workspace-write or
process-run authority may not mark any tool `ReadOnly` (ReadOnly
auto-allows at the approval gate), a tool's risk may not exceed its grant,
and a process-transport capability may declare `workspace:write`. A
non-empty process wire effect stages only when the host-canonical actual
intent is covered by the approved invocation bound; otherwise it is
refused before staging. Generic `shell.exec` / `process.run` /
`process.session` require Core-issued effect identity before spawn and
return a plain value; they never stage a prepared effect. The approved
bound is structured: `ExecArgv { program, argv }` for argv tools
(prefix cover with intact argument boundaries) and `ShellExec { dialect,
command_digest }` for `shell.exec` (exact RFC 8785 JCS digest of
`{"command"}`, never a shell-string prefix). Trusted `HostToolPolicy`
binds builtin arguments to those intents; `ToolSpec` is not authority
and a plugin cannot self-authorize via `ToolRisk` plus parameter names
(`command` / `argv` / `destination` / `payload`). `process.session`
poll/stop do not spawn and cannot spend an argv-prefix grant. Session
recovery is keyed by the start identity.

The complete execution authority binds the canonical resolved executable,
cwd scope and security-relevant environment used by dispatch. `ExecArgv`
authority covers the resolved executable identity, and the pre-spawn seal
recheck refuses a changed executable between approval and spawn
(`PROCESS-AUTHORITY-BOUND-01`, repaired in `f460558`/`13cf6c1`); cwd/PATH
shadowing and environment-controlled wrappers are therefore inside the
approved bound, not merely beside it.

Process-connection state is `HostLifecycle` (`NeverStarted` / `Serving`
/ `Quarantined` / `Stopped`): first connect is not a restart, and a
failed replacement stays quarantined. Capability start compares
`SandboxProfile` to post-spawn `SandboxCapabilities` (actual enforced,
not configured). `UntrustedGenerated` fails closed on the native process
plane; WASI is a V2 candidate for that profile, not a v0 slice. Do not
invent `MOD-18`.
A mismatched process-tool identity and a parent-escaping session cwd fail
closed before spawn. The grant
enforcement points are
under test: `undeclared_permissions_receive_no_handle` and
`capability_authority_is_derived_and_validated_at_registration`
(agent-runtime) prove the grant-by-construction behavior end to end, and
`sandboxed_self_check_artifacts_stay_contained` (agent-process) proves the
tested self-check writes its artifact into the private cwd. It is not an
escape-proof test for absolute filesystem or network access.

## 9c. The tool surface: merged meta-tools, bounded catalog, revisioned rounds

The always-visible tool schemas are themselves context. Since V1-M9 the
runtime control surface is two merged entry points instead of a dozen
single-purpose meta-tools:

- `context.manage` — `op` dispatch over catalog retrieval (`search` /
  `inspect` / `fetch`) and deliberate mutations (`tag` / `lease` /
  `admit` / `derive`). `item_id` accepts a bare UUID or the
  catalog uri `context://run/<uuid>` that search hits return. `gc_hint`
  and `collect` are not model-facing ops: the engine owns collection.
  `fetch` returns the catalog or stored body; catalog residency is not
  the selected working set. The tool never
  touches the engine.
- `capability.manage` — `op` dispatch over `search` / `inspect` / `load` /
  `unload`, provided identically by the builtin dispatcher and the
  capability-aware dispatcher (which filters out the builtin copy).

`task.complete` asks the model only for the bounded semantic summary. The
Runtime already owns current assistant-output and verification evidence and
attaches those handles to `CompletionRecord` at the safe point. The older
model-supplied artifact list remains parser-only compatibility; asking the
model to echo opaque runtime capabilities created avoidable invalid proposals
without adding authority or evidence. Ending a model turn is implicit and does
not close the durable task. Surface rev v5 keeps the compact `task.complete`
schema visible, but visibility grants neither closure intent nor authority.
Acceptance by the Runtime-owned completion gate records only a
`pending_terminal_commit` proposal in the active turn; the authoritative tool
result says so explicitly. After the whole sibling batch settles and the turn
crosses its durable barrier, Runtime re-derives the same completion decision
at the safe point and runs the terminal checkpoint transaction above. A failed
sibling, invalidated gate or pre-commit checkpoint failure keeps the task active
and projects one bounded typed failure into its resume state. An audit failure
after the terminal checkpoint is a committed completion plus a recovery fence,
not a retryable pending proposal.

The default always-loaded model surface is `fs.list`, `fs.read`, `fs.write`,
`search.grep`, `artifact.read`, `edit.patch`, `git.status`, `git.diff`,
`task.complete`, and `capability.manage`. `edit.patch` is the single canonical
revision-aware
mutation primitive for existing text; compact universal file creation and
read-only Git review remain visible because measured catalog-control rounds
cost more than their small schemas. Surface visibility grants no effect
authority. `edit.replace`, shell, process and plugin tools stay catalog-only.
`context.manage` is catalog-only until a typed evidence need
(Warm/Cold/Stored catalog, TaskAnchor `evidence_refs`, or open loops); the
model can also load it through `capability.manage`. Catalog search
accepts `role=mutate|verify|read_resource|search|inspect_diff|escape_hatch`
so the model does not have to guess keywords. The merge is evidence-backed,
not assumed: `merged_control_surface_costs_
fewer_schema_tokens` measures the merged schemas against the old
separate tools and asserts a decisive win.

The catalog is bounded, so a growing capability universe cannot itself
become context pollution:

- `capability.manage op=search` pages (default 20, capped at 50, with a
  name-sorted `cursor`) and spills the full listing to an artifact when the
  page is not the whole catalog — the model only ever sees the bounded
  page. Search ranks token-OR plus a phrase bonus, and may filter by
  `ToolSemanticRole` (`role=mutate` returns `edit.patch` without a text
  query).
- Registration validates and caps a capability's declared schemas:
  `MAX_TOOLS_PER_CAPABILITY` (32), `MAX_TOOL_NAME_CHARS` (64, names
  restricted to `[A-Za-z0-9._:-]`), `MAX_TOOL_DESCRIPTION_CHARS` (200),
  `MAX_TOOL_SCHEMA_BYTES` (4 KiB) — a single capability cannot blow up the
  model surface with one giant schema.

`ToolSurfaceSnapshot.generation` remains a legacy combined catalog display
value. The runtime no longer treats arithmetic combination as a unique round
identity: `ToolSurfaceSourceRevisions` preserves builtin catalog, capability
catalog, task requirement and focus revisions separately, while the monotonic
`surface_revision` identifies the final round projection. Execution-policy and
  TaskAnchor and execution-policy revisions are populated by the runtime;
  typed EpisodeOutcome revision remains absent until that authority plane
  exists. Zero is not used to pretend an unobserved source was sampled.

The registry never calls back into a capability object under its lock.
Registration reads and validates the manifest and tool schemas exactly
once (before the lock) and caches both on the entry; every catalog query
(`catalog`, `loaded_tool_specs`, `owner_of`, `tool_state`, `catalog_rows`,
...) reads the cache. A slow, re-entrant or panicking capability
implementation can only misbehave at register time — never while holding
the registry's `RwLock`.

## 9d. Durable turn commit + serialized capability lifecycle (V1-M10 start)

Three consistency gaps closed before V2:

**Turn finalization is a commit, not a fire-and-forget cleanup.** The
actor's `finalize_turn` walks `TurnState` — `Running` → `ModelFinished` →
`Committing` → `Committed` — and every mandatory state write must succeed
before the terminal event transaction is emitted: tool-observation ingest,
the `AfterTool` and `AfterModel` maintenance passes, the full GC, and their
journal events (`ContextMaintained`, `ContextGc`, `AssistantMessage`).
`TurnCompleted` and `RuntimeCommitBarrier(Turn)` are appended, flushed and
published as one bounded ordered batch; replay keys off the explicit marker,
not the lifecycle event name. On the first failure the commit aborts — later writes
would build on an inconsistent state — the turn frame is dropped, and the
runtime journals `TurnCommitFailed { phase, message }` (naming the exact
step) plus `RecoveryRequired` instead of pretending the turn completed.
"The model answered" and "the runtime durably committed this turn" are two
different facts; this is the foundation for crash recovery.

Each new run first durably writes `RunStarted` plus
`RuntimeCommitBarrier(RunStart)`. This opts the trace into explicit-marker
semantics before any turn can begin. If any marker exists, only markers advance
the committed replay prefix; completely marker-free historical traces alone
may infer legacy commits from `TurnCompleted`. Thus a first-turn partial batch
cannot become a false legacy commit merely because its final marker was the
member that failed to append. `RuntimeCheckpoint.event_cover_seq` is the event
cursor reflected by the frozen planes, not independent commit authorization.

The actor enforces that boundary with a process-local one-shot lifecycle:
`NotStarted | Serving | StartFailed`. Only a successful append **and flush** of
the complete startup batch enters `Serving`; every state-changing command is
rejected before then. A partial append or failed flush moves the actor to
`StartFailed` permanently, because retrying in place could let a later marker
authorize the earlier forensic prefix. Duplicate start is rejected, and
shutdown from `NotStarted`/`StartFailed` performs no Core stop, event append or
flush. Bounded read-only inspection remains available for diagnosis.

**Capability start/stop is serialized per capability.** The registry no
longer stores a `started: bool`; each entry carries a `CapabilityRunState`
(`Stopped` / `Starting` / `Started` / `Stopping` / `Failed`) and an async
`run_lock` held across the `start()`/`stop()` call. Two concurrent
`ensure_started` calls collapse into exactly one `start()`: the second
caller either observes `Started` on the fast path or waits on the lock and
re-checks. A failed start leaves the capability observably `Failed` and a
later start retries; `stop_all` takes the same lock, so a stop can never
race an in-flight start. The state is exposed on the catalog rows.

**Every `ContextEngine` method has process-boundary parity by test.** The
context-service wire gained `ServiceOp::StorageGc` (the conservative
storage GC is the only place information is deleted, so a wire gap there
would silently diverge). More importantly, `full_contract_parity_across_
the_process_boundary` drives a scripted lifecycle covering *every* contract
method — ingest, maintain, gc, materialize, scope lifecycle, diagnostics,
inspect, `search_external` / `inspect_external` / `fetch_external`,
`storage_gc`, `acknowledge_consumption`, checkpoint/restore — through both an
in-process engine and the service boundary, and asserts the normalized
outcomes are identical.
The checklist lives in one `contract_snapshot` helper: a new trait method
must be added there, and the parity test then verifies the wire op, the
service handling and the adapter override automatically. The integration
target is owned by the `agent-context-service` package and uses Cargo's
`CARGO_BIN_EXE_agent-context-service` path, so Cargo rebuilds the exact binary
whose wire contract the test drives. The old adapter dev-dependency cycle,
mtime freshness heuristic and manual build/touch workaround are gone.

**Trusted Core direction.** Core is not headed toward retirement by
merging into the runtime. Its turn-stateless primitives — permission/approval,
effect brokering, event/audit/durability, resource budgets, capability
authority, sandbox authority, runtime integrity — are the seed of an
`agent-core` the agent cannot modify, while everything evolvable (actor,
task manager, scope scheduling, prompt assembly, materialization, adaptive
policy) stays in `agent-runtime`. Runtime evolves, Core stays trusted.

Since the MOD-04 first slice (2026-08-11) the four authority seams have
one named home behind the `CoreAuthority` facade (`agent-core/src/
authority.rs`): `EventAuthority` (envelope identity + journal +
durability barrier), `ApprovalAuthority` (`ApprovalVerdict` normalization),
`EffectAuthority` (the single commit/rollback seam every staged effect
passes), and `OutputAuthority` (the only path from producer output to a
model-facing `ToolOutput`). This centralizes calls only — it is not yet
proof that opaque effects are safe — but it is the growing seam a future
Core implementation replaces without rewriting the facade or the actor.

## 9e. Performance P0: store confinement, lock-free IO, bounded external view

The next hot paths are store I/O, external recall and the tool surface —
not trait/vtable dispatch or small-object clones. The P0 items:

- **Context store out of the CWD.** The store's default fallback is no
  longer `.focus-agent/context-store` relative to the CWD; it is an OS
  temp dir scoped to the process. The composition root (TUI) and the
  context service both pin the store under `workspace.state_dir()/
  context-store` (the service receives it via `--store-dir`, threaded from
  `ContextServiceConfig.store_dir`), so externalized content can never
  scatter into a launch directory.
- **Sync disk IO out of the context lock.** `fetch_external` checks
  membership and captures the access-stamp inputs under the lock, reads
  the file outside it (async), then re-locks to stamp recency. Storage GC
  is a plan/IO/commit split like the full GC: the reachability closure and
  candidate decision run under the lock, `tokio::fs::remove_file` runs
  without it, and a fresh lock applies the outcomes. The state lock is
  never held across a disk read or write on the hot path.
- **External ContextMap is never fully cloned.** `materialize` surfaces a
  bounded view (`MAX_EXTERNAL_REFS = 32`, hot-entity / open-loop /
  checked-path / recency ranking; hot and Checked Stored hits come from
  the entity index, Warm hot/Checked from the eviction buffer, plus a
  recency tail — not a full-map clone); `search_external` truncates to the query limit
  before cloning (Resident/Warm hits are heap projections, not store
  clones); `inspect_external` returns one catalog descriptor. The full map
  stays in the engine. This bounds copied/model-facing data; ranking no
  longer walks the full map (CTX-07).

## 9f. Consistency invariant test suite

Tests that guard the runtime's consistency claims, worth more than any
extra scoring coefficient:

- runtime task id == context task id (`runtime_task_id_matches_the_
  context_task_id`); `CoreAuthority::set_focus` failure and `clear_focus` failure
  both leave the TaskManager untouched (`failed_focus_never_mutates_the_
  task_table`, `failed_clear_focus_never_mutates_the_task_table`);
  checkpoint → restore reproduces task ids, scopes and the current task
  with the engine focus aligned to the restored task;
- stale `PreparedEffect` normal-path test → target unchanged, staged temp
  deleted, `MutationRolledBack` journaled; injected cleanup, child-composite,
  review-terminal and Core operation-terminal faults return bounded recovery
  errors and fence later mutation. A landed rename whose journal record
  fails is reported `Applied + DurabilityFailed`, never "nothing
  happened"; every non-durable settlement strips proposed revisions;
- a `ContextAction` directive takes effect before the next model round
  (asserted on a shared activity timeline), not just at turn end;
- process-capability cancellation terminates the child (a heartbeat file
  the child rewrites stops advancing after cancel);
- `fetch(ref)` recovers the exact externalized content; the context store
  never writes outside its configured directory and the default fallback
  is never CWD-relative;
- dynamic vs service engines are parity-checked for every `ContextEngine`
  method (see 9d), and the assembled input + output reserve stays within
  the provider context window.

## 9g. Performance P1: indexed external map, bounded tool surface, cached catalog

- **The external map owns its indexes (`ExternalMap`).** `State.external`
  is no longer a bare `Vec`; it binds the entries with an id index and
  exact-entity buckets — the mirror of `ContextHeap`, and the same
  structural-mutation discipline (push / retain / take_all / replace_all
  rebuild the indexes in the same step; checkpoints serialize only the
  entries). `inspect_external` and the `fetch_external` membership +
  access-stamp path are O(1) id-index lookups instead of linear scans, so
  the model's retrieval loop (`context.manage` inspect/fetch) no longer
  costs O(map) per item. The GC's hot-entity recall answers exact matches
  from the entity buckets (O(bucket) per hot entity); substring-tolerant
  overlaps (hot `AuthService.rs` vs an entry entity `src/auth/AuthService
  .rs`) are not indexable with exact keys, so a residual scan over the
  entries the index did not propose keeps recall coverage identical.
- **`ContextCatalog` is the shared navigation directory.** Each `item_id`
  has exactly one body location (Resident / Warm / Stored). Query indexes
  (task / scope / kind / entity / label / residency / attention) serve
  both GC hot-entity recall and `context.search` candidate generation.
  Authority metadata stays on the body; checkpoints serialize the three
  stores and rebuild the directory. A free-text needle that hits no
  entity/label key still residual-scans summaries/uris. `label` is a real
  `ContextSearchQuery` dimension.
- **Retrieval access is graded (`CTX-GC-11`).** Search-hit, inspect, fetch,
  admit, and consumption ack are explicit ranks on the stored body
  (`AccessSignal`). Search may delay Cold -> External aging once until a
  stronger read, with per-item cooldown and a repeated-identical-query
  budget; it cannot pin never-used entries. Inspect/fetch reset search
  saturation. Ack is the strongest online signal (turn clocks +
  `access_count` + GC epoch) and never reactivates the body.
- **Federated discovery is an internal planner (`CTX-DISC-01..03`, `TOOLS-10`).**
  `ResourceRef` / `ResourceDescriptor` / `DiscoveryMiss` are the shared card.
  `context.manage` and `capability.manage` keep their public schemas; there
  is no `runtime.search` tool. Search is read-only (no admit, no load).
  Capability search uses a provider-owned token index over descriptor
  fields. Per-turn query/identical-query caps live on the actor.
  Artifact/Task/Agent/Skill/Event providers are not in this prototype;
  inspect-by-id does not yet take a revision.
- **Retrieval is metered on the event stream (M15 instrumentation).**
  `ContextGcReport.externalized_ids` and `ContextDiagnostics` access-stamp
  counters let `agent-eval` join forgotten ids to later search/inspect/
  fetch/admit and report search latency plus reinforcement distribution.
  `agent-eval --retrieval` is the engine-only found-after-forgotten
  baseline. This is not the paired real-model coding acceptance.
- **The materialized external view is bounded at the type level.**
  `MaterializedContext.external` is now `ContextMapView`
  (`CONTEXT_MAP_VIEW_CAP = 32`): the constructor asserts the cap and the
  `Deserialize` impl rejects over-cap wire data, so the bound holds on
  both sides of the context-service boundary. It serializes transparently
  (same wire shape as the raw refs), and the selection side keeps the
  quickselect ranking inside the engine.
- **The tool surface has a deterministic schema budget.** Each round's
  surface is bounded at capture (`MAX_TOOL_SURFACE_TOKENS = 4096`):
  control tools and the base catalog's core tools are never trimmed;
  optional tools (loaded builtin + capability tools) are kept
  smallest-schema-first (name tie-break) until the cap. Budget, prompt
  and tool-call validation all read the same bounded snapshot, so pricing
  is honest; a loaded tool trimmed off a round gets an explicit "not on
  this round's model surface" error instead of "unknown tool". The final
  budget guard stays as the backstop for tiny provider windows.
- **Capability catalog metadata is cached.** `catalog_rows()` returns an
  `Arc` of the derived discovery rows, rebuilt only when
  `catalog_version` changes (register / activation / load / unload /
  active-marks / restore) — distinct from the audit `generation`, so a
  tool executing mid-round cannot churn the snapshot's audit identity. An
  unchanged catalog answers `capability.search` without re-reading the
  registry and re-cloning every tool description.

## 9h. Performance P2: scope tree index, top-K selection, typed edges, batched IO

- **The scope tree owns its id index (`ScopeTree`).** `State.scopes` is no
  longer a bare `Vec`; the tree binds the scopes with an id index. Scope
  ids are immutable, so the only structural mutation is `push` (insert at
  slot + index in one step); close/ancestor lookups (`close_scope`,
  `nearest_open_parent`, `in_scope_chain`, `belongs_to`'s parent walk) go
  through `by_id` / `index_of` in O(1) instead of re-scanning per hop.
  Checkpoints serialize only the scopes; the index rebuilds on restore.
- **The materializer selects by top-K, not full sorts.** Candidate
  ordering is deterministic (score descending, slot as the tie-break —
  the old unstable sort left equal-score order undefined), and when the
  caller caps the working set (`max_selected_items`) the candidate
  universe is quickselect-trimmed to that bound *before* sorting
  (O(n + k log k) instead of O(n log n); the cap also bounds how many
  items can be selected, so trimming cannot change the outcome).
  Dependency expansion pops a bounded max-heap
  (`ExpandedCandidate`, top-8 window) instead of sorting the whole
  expanded set.
- **Dependency edges are typed.** `ContextItem.dependencies` (and the
  externalized entry's captured edges) are now `Vec<DependencyEdge>`
  (`{ target, kind }`) instead of bare ids: the graph records *why* an
  item is referenced, so GC reachability and future supersession/evidence
  policies can distinguish edge semantics. The `Deserialize` impl accepts
  the pre-typed bare-id form, so old checkpoints keep loading.
  `ContextItemSummary` stays a projection (target ids only), so replay and
  the UI are untouched.
- **Store IO batches concurrently.** The GC's IO phase and the Storage GC
  run their file writes/reads/removals on a `JoinSet` instead of one
  await per file: the lock-free window shrinks to the slowest single
  operation instead of the sum. Each overflow item gets its own write
  attempt, so a transient failure no longer forfeits the rest of the
  batch.

## 10. Explicit non-goals for v0.1

- vector embeddings;
- vector DB;
- RAG;
- graph memory;
- learned ranking;
- multi-agent orchestration;
- autonomous self-modifying policies;
- IDE-scale file index;
- full provider abstraction matrix;
- distributed runtime.

These are deliberately excluded so the first experiments isolate dynamic context lifecycle behavior.
