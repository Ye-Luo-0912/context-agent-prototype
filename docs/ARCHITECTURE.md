# Initial Architecture

## 1. Purpose

This prototype is not intended to become a second ContextCore. It is a small, real agent runtime used to validate the runtime behavior that ContextCore will eventually power.

Primary hypothesis:

> Long-running agents should continuously maintain a bounded, task-focused working set. Completed or low-value information should leave active context during execution, not remain in an append-only transcript until a context-window threshold forces compression.

The first version therefore prioritizes runtime/context boundaries over model features.

## 2. Layering

```text
┌─────────────────────────────────────────────────────────────┐
│ Presentation                                                │
│ agent-tui                                                   │
│ RuntimeEvent -> RunStateAggregator/AppState -> TUI          │
└─────────────────────────────┬───────────────────────────────┘
                              │ User command / RuntimeEvent
┌─────────────────────────────v───────────────────────────────┐
│ Runtime                                                     │
│ agent-kernel                                                │
│ Agent loop / budgets / approval / event publication        │
└──────────────┬───────────────────┬──────────────────────────┘
               │                   │
     ContextEngine          ToolDispatcher / ModelTransport
               │                   │
┌──────────────v───────┐  ┌────────v─────────────────────────┐
│ Context Selection    │  │ Execution                       │
│ context-simple       │  │ tool-runtime                    │
│ working set          │  │ fs / shell                     │
└──────────────────────┘  └────────┬─────────────────────────┘
                                  │
                         ┌────────v─────────────────────────┐
                         │ agent-workspace                  │
                         │ cwd boundary / artifacts         │
                         └──────────────────────────────────┘

RuntimeEvent ───────────────────────────────> agent-storage
                                               JSONL files
```

## 3. Dependency direction

The intended dependency direction is strict:

```text
agent-contracts
      ^
      ├──────── context-simple
      ├──────── agent-workspace
      ├──────── tool-runtime
      ├──────── agent-storage
      ├──────── agent-process          (framed IPC / child lifecycle / sandbox)
      └──────── agent-kernel
                    ^                  ├── agent-capability-process -> agent-process
                    │                  └── context-contextcore      -> agent-process
                 agent-tui
```

Important consequences:

- `agent-kernel` does not import `context-simple`.
- `tool-runtime` does not import `context-simple` or any memory implementation.
- `agent-tui` is the composition root and chooses concrete implementations.
- `context-contextcore` implements `ContextEngine` over a process without
  changing the kernel; the framed transport it uses is the shared
  `agent-process` host, so every process boundary (context service and
  process capabilities) speaks one framing/deadline/sandbox policy.
- Future `context-contextcore` can implement `ContextEngine` without changing the kernel.

## 4. Stable contracts

### ContextEngine

The kernel needs a small fixed surface:

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
   deterministic retrieval surface for externalized refs (default no-ops,
   so engines without a store keep working). Search filters the indexed
   dimensions of the external map (entity signature, kind, scope, task);
   inspect returns one entry's metadata without a store read; fetch pulls
   the full content back. See `docs/CONTEXT_LIFECYCLE.md` §9g.

The API is asynchronous even though `context-simple` is in-process. This leaves room for a future ContextCore service adapter over local IPC/HTTP/gRPC without changing the kernel contract.

Since V1-P0-2 the engine never renders prompt text. It answers a
`ContextQuery { current_input, budget_tokens, hints }` with a
`MaterializedContext { focus, items, external, selected, approx_tokens,
diagnostics }`; `items` are structured `MaterializedItem`s, `external` is
a bounded refs-only view of the store (max 32 entries, never the full
map), and the runtime-owned `PromptAssembler` is the only place that turns
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

The kernel never knows which one it runs; the composition root selects it
(`agent-tui --context=append|rolling|dynamic|service`). `agent-replay
--compare` replays the same scripted scenarios through all three and reports
token cost and churn (`docs/EXPERIMENTS.md`).

`SimpleContextEngine` (C) also implements the P4 explicit features:
decision supersession and the error lifecycle (persist → recurring
supersession → verified-fixed archive). Both are configurable and emit
explainable `ContextStateTransition`s (`docs/CONTEXT_LIFECYCLE.md` §9b).

### ModelTransport

The model provider is deliberately outside the kernel. A provider adapter translates `ModelRequest`/`ModelOutput` to a vendor API.

The kernel never depends on provider-specific response IDs, SDK types, or streaming structures.

The P1 contract adds:

- `capabilities()` — `ModelCapabilities` (streaming, tool calls, max output
  tokens) so the UI/runtime can branch without vendor knowledge.
- `complete_stream(request, sink)` — the kernel always drives the model through
  this. The provider normalizes vendor wire chunks into `ModelChunk`
  (`TextDelta`, `ToolCallDelta`, `Done`) delivered to a `ModelEventSink`, and
  returns the final assembled `ModelOutput`. A default implementation bridges
  a non-streaming `complete` into a single delta, so every transport works in
  the streaming loop.
- Cancellation — `ModelRequest.cancel` is a `CancellationToken`; the provider's
  stream loop `select!`s on it and aborts with `AgentError::Cancelled`. The
  kernel exposes `cancel_current_turn()`, checks the token between tool rounds,
  and ends a cancelled turn cleanly (`Warning` + `TurnCompleted`).
- Streaming deltas are live-only: `RuntimeEvent::ModelDelta` is broadcast to
  UI subscribers but never journaled — the final `AssistantMessage` carries
  the complete content for replay. Since V1-M9 each delta carries
  `turn_id`/`operation_id`/`generation` and the UI's `RunStateAggregator`
  accepts deltas only for the operation it currently renders, so a late
  delta from a cancelled turn can never leak into the next turn's view —
  the live stream is fenced the same way the final `OperationResult` is.
- Retry/backoff lives at the transport boundary: `provider-openai` ships a
  generic `RetryingTransport` wrapper that retries only
  `AgentError::Transport { retryable: true }` errors (network, timeout, 5xx,
  429) with exponential backoff.

`provider-openai` speaks the OpenAI Chat Completions SSE protocol, which
DeepSeek, Qwen, Moonshot/Kimi, GLM, and most vendors also implement: point
`OpenAiConfig::base_url` at any of them. All vendor wire parsing stays in that
crate.

### ToolDispatcher

Tools receive a `ToolExecutionRequest` and return `ToolOutput`.

They do not receive a ContextEngine, memory store, or conversation transcript.

The trait has four members: `specs()`/`snapshot()` (pure — the stable model
surface for one round), `gc()` (the explicit lifecycle safe point the runtime
calls once per model round; default no-op) and `execute`. A model round uses
exactly one `ToolSurfaceSnapshot` (`specs` + `generation`) for the budget,
the prompt and tool-call validation, so a tool GC'd between the budget and
the call can never change what the model saw.

Since V1-M9 the lifecycle contract is unified for builtin tools and dynamic
capabilities under one catalog:

- `catalog()` — every known tool (builtin + capability) with lifecycle state
  and owner, for `capability.search` / `capability.inspect`;
- `load_tool(name)` / `unload_tool(name)` — move a tool (or the capability
  owning it) on/off the model surface;
- `inspect_tool(name)` — one tool's full spec;
- `ToolLifecycle::{Available, Loaded, Active, Warm, Unloaded}` is the one
  lifecycle shared by both planes: registered-but-not-loaded capability
  tools are `Available` and do not grow the prompt, the builtin catalog
  cools `Loaded -> Warm -> Unloaded` on idle ticks, and the unified control
  tools (`capability.search/inspect/load/unload`, always visible) drive the
  active set.

Since V1-M9 the model can also steer the *context* surface through
read-only meta-tools (`context.gc_hint` / `context.tag` / `context.lease` /
`context.collect`, always loaded with the core set). They do no work
themselves: each returns a `ToolOutcome::RuntimeDirective` carrying a typed
`ContextAction` (`Collect` runs the GC pass via `ContextEngine::gc`; the
rest become a `ContextDirective` ingest) — tools still never touch the
engine or memory stores (invariant 3), and the kernel stays the only
authority over how a directive is applied. The model addresses items by the
ids exposed in the materialized context frame (`id=<...>` per item), and
the engine silently ignores directives whose target item is gone.

Two guards keep these directives from becoming a backdoor:

- **commit-time execution**: the directive is executed by the actor when
  the tool result commits (inside the operation's generation fence), not
  at turn finalize — `context.collect` really collects *now*, and a hint
  lands before the next model round observes it. Finalize only persists
  observations;
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
- `Workspace::resolve_mutation` adds a hard rejection of the runtime
  state directory (`.focus-agent/` — traces, checkpoints, artifacts,
  change journal); mutating tools (`fs.write`, `edit.replace`) resolve
  through it, so ordinary coding tools cannot overwrite runtime state.
- Read tools (`fs.list`/`fs.read`/`search.grep`) use `resolve_relative`
  and can still read artifacts.

### Mutation transactions

Since V1-P0-6 every file mutation is a `MutationTransaction` produced by
`Workspace::begin_mutation`:

```text
resolve_mutation → capture old content (bounded) → stage temp file in
target dir → record change journal → atomic rename (swap)
```

The journal entry lands *before* the swap, so a journal failure never
leaves the target half-mutated (a retrying agent cannot double-apply an
already-landed mutation); any failure removes the temp file and leaves
the target untouched. `fs.write` and `edit.replace` both use it —
`fs.write` now journals every write instead of bypassing the change
journal.

Since V1-M9 the journal is tri-state so recovery can tell what actually
happened:

```text
MutationPrepared { tx_id, target, before_hash, after_hash }
        │
        ├─ atomic rename ok → MutationCommitted { tx_id }
        └─ rename failed / stale operation → MutationRolledBack { tx_id }
```

`before_hash`/`after_hash` are content hashes captured at prepare time,
so a later recovery pass can distinguish "prepared but never committed"
(no rename landed) from "committed" (the target now carries
`after_hash`). The single-record variant that claimed a mutation without
proof is gone: a rename failure now rolls the transaction back instead of
leaving the journal describing a mutation that never landed.

Commit failures are structured, because "the file did not change" and
"the file changed but I could not record it" need different recovery:
`EffectCommitError::NotApplied` (rename never landed — target intact) vs
`EffectCommitError::AppliedButDurabilityFailed` (the swap landed but the
`MutationCommitted` record could not be appended — the runtime must treat
this as a degraded state needing recovery, never report "no change" to
the model). The swap itself goes through `agent-workspace`'s
`atomic_replace(src, dst)` primitive — a true atomic-overwrite on both
platforms (Unix `rename`, Windows `MoveFileExW` with replace + write
through), never a remove-then-rename that breaks atomicity.


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

`agent-kernel` owns the `ApprovalGate` contract. Two implementations exist:

- `PolicyApprovalGate` — automatic policy (`read_only()`, `permissive()`, or a
  custom mix). No UI involved.
- `InteractiveApprovalGate` — used by the TUI. Works with an `ApprovalBroker`
  (shared hub): the kernel side broadcasts an `ApprovalRequest` (request id +
  tool call + spec) and waits on a oneshot; the UI subscribes to the broker,
  shows the tool name and a bounded args preview, and answers y/n/Enter/Esc
  through `InteractiveApprovalGate::respond`. Late subscribers can drain
  `broker.pending()`.

Read-only tools always auto-allow. The wait for an answer is bounded
(default 5 minutes, configurable): if nobody responds, the request is denied
and the pending queue is cleaned, so a missing responder can never hang a
turn. `agent-tui` runs in interactive mode by default; `--read-only` selects
`PolicyApprovalGate::read_only()`.

```text
kernel execute_tool
   │  authorize(call, spec)
   ▼
InteractiveApprovalGate ── broadcast ──► ApprovalBroker ──► TUI prompt (y/n)
   │  (oneshot wait, bounded)                                  │ respond(id, decision)
   └────────────────────────◄──────────────────────────────────┘
   ▼
ToolDispatcher.execute(ToolExecutionRequest { cancel, .. })
```

### EventJournal

Runtime events form the learning/replay substrate. The initial implementation is an append-only JSONL journal. This records what the runtime actually did without turning governance/report data into the online hot path.

## 5. Agent loop

```text
User input
   │
   ├─ emit UserMessageAccepted
   ├─ ContextEngine.ingest(UserMessage)
   ├─ maintain(UserInput)
   │
   v
maintain(BeforeModel)
   │
   v
ContextEngine.materialize(ContextQuery)   ── the Context Frame (long-term working set)
   │
   v
PromptAssembler.assemble() = System Policy + Focus Frame + Context Frame + Turn Frame + Tool Schemas
   │
   v
ModelTransport.complete_stream()
   │
   ├─ final answer ──────────────────────┐
   │                                     │
   └─ tool calls                         │
       │                                 │
       ├─ approval                       │
       ├─ ToolDispatcher.execute         │
       ├─ artifact raw output            │
       ├─ bounded ToolOutput ──▶ Turn Frame (execution stack, runtime-owned)
       └─ not ingested during the turn   │
             │                           │
             └──── next model ───────────┘

Final answer
   ├─ persist turn: ingest(ToolObservation) x N + maintain(AfterTool)
   ├─ ingest(AssistantMessage)
   ├─ maintain(AfterModel)
   └─ TurnCompleted
```

The tool result loop is the runtime's execution stack, not long-term memory:
results ride in the `TurnFrame` and never touch the context engine until the
turn ends, when they are persisted as observations and observed by one
`maintain(AfterTool)` pass.

### The model budget: the engine only sees its slice

Since V1-P0-7 the budget handed to `ContextQuery::budget_tokens` is the
residual of a full request budget computed at the runtime top level
(`agent-runtime::budget::ModelBudget`):

```text
Provider Context Window (ModelCapabilities.context_window, falls back to
                         the kernel's configured budget)
        - Output Reserve        (max_output_tokens, or a default reserve)
        - System Policy         (the assembled system prompt)
        - Turn Frame            (wire-form estimate of the turn stack)
        - Active Tool Schemas   (wire-form estimate of the tool specs)
        = Context Frame Budget  (the only number the engine receives)
```

The engine never sees the window, the output reserve or the tool schemas —
it just knows it has N tokens for the working set. The current user input is
charged inside the turn frame (it rides there), so the engine does not
deduct it a second time; the focus frame stays engine-owned. Pinned items
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
  `context_window - output_reserve` — the *input* budget. Rendering
  overhead may never eat into the space reserved for the answer. When the
  context frame is emptied and the fixed layers (system + turn + tools)
  still overshoot, the runtime auto-unloads the largest optional tools
  (core tools refuse) and re-snapshots; a request that still does not fit
  is a **hard error** — the runtime refuses to send instead of silently
  over-budgeting the provider.

## 6. Context is rebuilt, not replayed

The model-facing request is intentionally rebuilt from state instead of
replaying an append-only transcript. The input is assembled in five layers:

```text
System Policy    - standing instructions (runtime-owned)
Focus Frame      - the current task/goal, rendered from the materialized focus
Context Frame    - the selected working set, rendered from MaterializedItem's
Turn Frame       - the current turn's execution stack (runtime-owned)
Active Tool Schemas - tool definitions for this request
```

The split is deliberate. The context engine owns the long-term working set
and knows nothing about the execution protocol; the runtime owns the turn
stack and never scores or evicts it while the turn is open. Since V1-P0-2
prompt rendering lives in one place only: `PromptAssembler` turns the
engine's structured `MaterializedContext` into the five-layer input. The
engine could not format a prompt even if it wanted to — it never sees the
system prompt or the tool schemas.

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
runtime actor: opened at tool start (`ContextEngine::open_scope`), closed
when the next model round consumes the result (`ContextEngine::close_scope`);
the observation persisted at turn end stays tagged with the tool scope id.
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

`tool-runtime` registers eight tools behind the `Tool` trait
(`execute(run_id, call_id, arguments, cancel)`). Every tool defines an
explicit model-content budget and artifact policy:

| Tool | Risk | Model sees | Raw output |
| --- | --- | --- | --- |
| `fs.list` / `fs.read` / `fs.write` | read / write | bounded listing/content | file |
| `search.grep` | read | ≤ 100 hits (`file:line: content`) | artifact when more |
| `edit.replace` | write | diff + result summary | change journal |
| `git.status` / `git.diff` | read | ≤ 12 K chars tail | artifact when truncated |
| `shell.exec` | process | 200-line ring tail, ≤ 12 K chars | artifact (incremental append) |

`search.grep` skips `.git`, `.focus-agent`, `target`, `node_modules`, `vendor`,
`dist`, `build`, `.idea`, `.vscode` and caps files scanned (5000) and bytes per
file (2 MB). `shell.exec` streams stdout/stderr through two reader tasks into a
bounded channel (512), kills the child on timeout/cancel, and appends the full
log incrementally to an artifact via `Workspace::create_artifact`.

### Workspace change journal

Mutating tools record a `WorkspaceChange`
(tool/path/action/byte sizes/old content when ≤ 256 KB) to
`.focus-agent/changes.jsonl` via `Workspace::record_change`. The journal is the
review/revert substrate for the "mutating actions are visible and
reversible/reviewable" acceptance criterion, without putting raw file content
into context.

## 8. UI model

The TUI does not render internal kernel objects directly.

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
so context behavior stays observable without binding widgets to kernel or
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
context engine is no longer a complete snapshot: the runtime's checkpoint is
a `RuntimeCheckpoint` (versioned) wrapping the task manager (task rows +
current task id), the context checkpoint, capability activation state and
store generation/refs, and `RuntimeInstance::restore` puts the whole runtime
— task table included — back together. The `TaskManager` applies its
transitions transactionally: it validates and *prepares* a transition, the
external side (kernel focus/context) commits first, and only then does the
manager commit — a failed `set_focus` never leaves the task table changed.

Focus/clear/complete are multi-step context transactions: the kernel takes a
portable engine checkpoint before ingest + maintenance and restores it on
either failure; the actor commits task authority only after that succeeds,
then publishes audit/UI events. Restore validates the redundant active-task
fields and restored engine focus before exposing the task table, and applies
capability flags last. If context rollback itself fails, the actor fences
further mutation and emits `RecoveryRequired` until a known-good full restore
succeeds. An audit-event failure after aligned state commits is handled the
same way: state stays aligned, but the missing record is an explicit recovery
gap rather than a retryable "nothing happened" result. Cross-plane *capture*
is not frozen yet (actor state and the shared
capability registry are sampled separately); `docs/AUDIT_TODO.md` CORE-03
tracks that remaining gap.

## 8c. Runtime actor and module host (V1-M3, hardened V1-P0-1/V1-P0-4)

Since V1-M3 the runtime is an actor (`agent-runtime`), not `Mutex`
orchestration. Callers hold a cloneable `RuntimeHandle`:

```text
RuntimeHandle ── mpsc<RuntimeCommand> ──▶ RuntimeActor (owns mutable state)
                     │                        │
                     │                        └── model/tool operations
                     │                            └─▶ OperationResult
                     └──── events ◀── broadcast channel (kernel events)
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
  scope/operation ids + generation); the actor validates the generation
  and only then commits (turn-frame push, context ingest/maintenance,
  events). Stale results are dropped before they can change runtime
  state — `is_stale` no longer protects only "after the whole kernel
  turn ran";
- since V1-M9, tool side effects are two-phase. Tool *computation* and
  tool *side-effect commit* are separate: `ToolOutcome` is either a
  plain `Value(ToolOutput)` or a `PreparedEffect { output, effect }`
  where the effect is a staged, rollback-able mutation (today:
  `agent-workspace`'s `PreparedMutation` with its journal
  transaction). The actor checks the generation *between* the two
  phases — a stale tool operation has its prepared effect rolled back
  (temp file removed, `MutationRolledBack` journaled) instead of
  committed, so an external side effect cannot slip through the fence
  that protects model state;
- `AgentKernel` is now a stateless executor/helper: context/model/tool
  primitives plus event plumbing (journal, sequence, broadcast). Its
  turn loop, turn locks and `TurnFrame` ownership are gone;
- since V1-M9, a tool result can carry a context directive: the actor
  executes the `RuntimeDirective` at operation-commit time, inside the
  same generation fence that guards effect commit — `Collect` runs
  `ContextEngine::gc()` immediately and emits `RuntimeEvent::ContextGc`,
  everything else becomes a `ContextDirective` ingest, so a hint/lease/tag
  lands before the observation it targets (see the meta-tools under §4
  ToolDispatcher);
- every completed tool result passes a runtime-owned 16,000-character
  model-content guard before it can enter TurnFrame, context or events.
  Normal producers still spill the full result to an artifact; the guard is
  defense against a capability/adapter violating that contract;
- the actor selects on both the command channel and the operation
  completion channel, so `/cancel` is processed mid-operation and a new
  turn can start right after; cancellation is committed by the actor
  immediately (warning + TurnCompleted).

Composition uses a module host over typed capabilities:

```text
ModuleHost ── add_module (register + validate) ──▶ ServiceRegistry (typed lookup)
   │  ContextModule / ModelModule / ToolModule / ApprovalModule /
   │  EventModule / ArtifactModule
   └── start transactional, stop: capabilities first, then modules reverse
```

There is no universal `handle_event`: modules publish typed capabilities
(`ContextService`, `ModelProvider`, `ToolProvider`, `ApprovalPolicy`,
`EventStore`, `ArtifactStore` — all `CapabilityProvider` markers in
`agent-contracts`) and consumers look them up by type. The TUI composes the
run through the host and reads the capabilities back into the kernel.

Since V1-M9 the host lifecycle is transactional. Start is all-or-nothing
in order (a failing module rolls back the already-started ones), and stop
is dependency-safe and best-effort: dynamic capabilities are stopped
*first* — a capability that depends on a typed service must die before
the service it uses — then the typed modules in reverse order, and every
stop error is aggregated into one result instead of aborting at the
first failure. `RuntimeInstance` already aggregates Runtime/Host/Actor
layers; the host applies the same rule inside its own plane.

Since V1-P0-4 the host is an extension platform with two planes:

- **Trusted core (typed).** `ServiceRegistry::register` / `get` are public,
  so external crates publish and retrieve typed services with their own
  `CapabilityId`s — the core ids are the well-known set, not the only set.
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
                     kernel tool_provider -> model tool schemas -> invoke
```

Since V1-M9 capability invocation returns a `CapabilityOutcome`, not a raw
`ToolOutput`: `Value(ToolOutput)`, `EffectRequest { output, effect }`
(a staged, rollback-able mutation the core commits under the generation
fence — external capabilities submit *requests*, they do not perform side
effects themselves), or `RuntimeDirective { output, directive }` (the
context-control path, gated on the `runtime:context-control` manifest
permission). The dispatcher maps these onto the kernel's `ToolOutcome`,
so the effect fence is the same for builtin prepared mutations and for
dynamic capabilities — there is no second, unfenced side-effect lane.

Since V1-P0-8 the composition root composes into one `RuntimeInstance`
that owns the `ModuleHost`, the `RuntimeHandle` and the actor `JoinHandle`.
Shutdown is a single ordered step with aggregated errors:

```text
runtime.shutdown()
  cancel any turn
    → stop the actor (kernel stop: flush journal, emit RunCompleted)
    → stop the module host (dynamic capabilities first, then
      typed modules reverse — each step best-effort, errors aggregated)
    → join the actor task
    → aggregate errors
```

The actor itself never returns silently: `Stop` replies with the kernel
stop result, and the "all handles dropped" path (`rx.recv() -> None`)
runs the same teardown so journal flush and `RunCompleted` do not depend
on the caller remembering to stop.

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
`agent-context-service` behind the same protocol; the kernel, tools,
approvals, TUI and provider are untouched (`agent-tui --context=service`).

Do not move Agent Kernel, tools, approvals, TUI, or provider code into ContextCore merely because ContextCore supplies context selection.

## 9b. Process capability boundary: sandbox + cancellation (V1-M9)

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
- **Dedicated cwd** — `cwd: Option<PathBuf>`: the child runs in its own
  directory, created at connect, never the parent's cwd — a generated
  capability cannot roam the workspace by relative paths.
- **Process/job limits and CPU quota** — Unix `pre_exec` rlimits
  (`RLIMIT_CPU`, `RLIMIT_NPROC`) applied right after fork; the adapter
  sets 60 s CPU and 16 processes.
- **Kill tree on cancel** — `call_with_cancel(op, cancel)` selects the
  framed request against the invocation's `CancellationToken`: on cancel
  (user `/cancel`, superseded operation) it poisons the connection and
  kills the whole process tree immediately — Unix: SIGKILL to the process
  group the child was spawned into (`process_group(0)`); Windows:
  `taskkill /PID <pid> /T /F`. A cancelled capability stops producing
  side effects *now*, not at the request deadline.
- **Bounded stderr** — `stderr_capture_bytes`: when set (the capability
  profile uses 64 KiB), the child's stderr is piped and drained by a task
  into a bounded ring kept on the host; `ProcessHost::stderr_tail()`
  surfaces the newest bytes for diagnostics. A chatty child can no longer
  inherit unbounded output into the parent console.
- **Brokered host APIs** — the child only ever speaks the bounded
  JSON-lines ops (`ping`/`invoke`); every host side effect goes through
  the same framed, deadline-bounded, poisoned-connection protocol as the
  context service.

`ProcessCapabilityAdapter::from_manifest` applies this strict profile to
every process capability; `invoke` forwards the call and the granted
permissions (informational), but the sandbox — not the manifest — is what
enforces the boundary. Since the trust-boundary hardening, the manifest
itself is no longer trusted either: the id must pass a conservative
grammar (`validate_capability_id`, lowercase/digit start, `[a-z0-9._-]`,
<= 64) before it is embedded in the working directory or any route, and
the working directory is private and unpredictable
(`context-agent-capability-<id>-<uuid>`) so no two runs share a path and
a hostile pre-created directory cannot be predicted. Filesystem isolation
beyond the dedicated cwd and an explicit network policy are still open (a
future dedicated capability sandbox); until then **V2 autonomous
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
`Effect` the core commits behind the generation fence — a capability
mutation can no longer land during `invoke` (the CORE-01 bypass). Risk is
derived, never self-declared: a capability declaring workspace-write or
process-run authority may not mark any tool `ReadOnly` (ReadOnly
auto-allows at the approval gate), a tool's risk may not exceed its grant,
and a process-transport capability declaring `workspace:write` is refused
until the wire-level effect broker exists. Both enforcement points are
under test: `undeclared_permissions_receive_no_handle` and
`capability_authority_is_derived_and_validated_at_registration`
(agent-runtime) prove the grant-by-construction behavior end to end, and
`sandboxed_self_check_artifacts_stay_contained` (agent-process) proves
the V2 loop's test step — a generated capability's self-check — runs
inside the sandbox and its artifacts cannot escape the dedicated cwd.

## 9c. The tool surface: merged meta-tools, a bounded catalog, one generation (V1-M9)

The always-visible tool schemas are themselves context. Since V1-M9 the
runtime control surface is two merged entry points instead of a dozen
single-purpose meta-tools:

- `context.manage` — `op` dispatch over the four directives (`gc_hint` /
  `tag` / `lease` / `collect`, producing a `RuntimeDirective`) and the
  three retrieval queries (`search` / `inspect` / `fetch`, producing an
  `EngineQuery`). One schema, one description, same invariants: the tool
  never touches the engine.
- `capability.manage` — `op` dispatch over `search` / `inspect` / `load` /
  `unload`, provided identically by the builtin dispatcher and the
  capability-aware dispatcher (which filters out the builtin copy).

The default model surface is now five schemas — `fs.list`, `fs.read`,
`search.grep`, `context.manage`, `capability.manage` — down from fourteen.
The merge is evidence-backed, not assumed: `merged_control_surface_costs_
fewer_schema_tokens` measures the always-visible schema cost of the merged
surface against the old separate tools and asserts a decisive win.

The catalog is bounded, so a growing capability universe cannot itself
become context pollution:

- `capability.manage op=search` pages (default 20, capped at 50, with a
  name-sorted `cursor`) and spills the full listing to an artifact when the
  page is not the whole catalog — the model only ever sees the bounded
  page.
- Registration validates and caps a capability's declared schemas:
  `MAX_TOOLS_PER_CAPABILITY` (32), `MAX_TOOL_NAME_CHARS` (64, names
  restricted to `[A-Za-z0-9._:-]`), `MAX_TOOL_DESCRIPTION_CHARS` (200),
  `MAX_TOOL_SCHEMA_BYTES` (4 KiB) — a single capability cannot blow up the
  model surface with one giant schema.

`ToolSurfaceSnapshot.generation` is unified: the builtin catalog's
generation (its load/unload/gc transitions) is combined with the
capability registry's own counter, which bumps on every capability surface
change — `register` / `set_activation` / `load_tool` / `unload_tool` — so
a dynamic capability change is auditably visible in the snapshot instead of
masquerading as the builtin catalog's generation.

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
before `TurnCompleted` is emitted: tool-observation ingest, the `AfterTool`
and `AfterModel` maintenance passes, the full GC, and the journal events
for each (`ContextMaintained`, `ContextGc`, `AssistantMessage`,
`TurnCompleted`). On the first failure the commit aborts — later writes
would build on an inconsistent state — the turn frame is dropped, and the
runtime journals `TurnCommitFailed { phase, message }` (naming the exact
step) plus `RecoveryRequired` instead of pretending the turn completed.
"The model answered" and "the runtime durably committed this turn" are two
different facts; this is the foundation for crash recovery.

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
`storage_gc`, checkpoint/restore — through both an in-process engine and
the service boundary, and asserts the normalized outcomes are identical.
The checklist lives in one `contract_snapshot` helper: a new trait method
must be added there, and the parity test then verifies the wire op, the
service handling and the adapter override automatically. The service is a
dev-dependency of the adapter crate, so the binary is rebuilt whenever the
wire protocol changes — a stale service can never masquerade as the
current protocol.

**Trusted Core direction.** The kernel is not headed toward retirement by
merging into the runtime. Its stateless primitives — permission/approval,
effect brokering, event/audit/durability, resource budgets, capability
authority, sandbox authority, runtime integrity — are the seed of an
`agent-core` the agent cannot modify, while everything evolvable (actor,
task manager, scope scheduling, prompt assembly, materialization, adaptive
policy) stays in `agent-runtime`. Runtime evolves, Core stays trusted.

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
  bounded view (`MAX_EXTERNAL_REFS = 32`, hot-entity/open-loop/recency
  ranking via quickselect); `search_external` truncates to the query limit
  before cloning; `inspect_external` clones one entry. The full map stays
  in the engine. This bounds copied/model-facing data, not CPU: ranking still
  scans and collects O(total refs) borrowed entries pending CTX-07.

## 9f. Consistency invariant test suite

Tests that guard the runtime's consistency claims, worth more than any
extra scoring coefficient:

- runtime task id == context task id (`runtime_task_id_matches_the_
  context_task_id`); `kernel.set_focus` failure and `clear_focus` failure
  both leave the TaskManager untouched (`failed_focus_never_mutates_the_
  task_table`, `failed_clear_focus_never_mutates_the_task_table`);
  checkpoint → restore reproduces task ids, scopes and the current task
  with the engine focus aligned to the restored task;
- stale `PreparedEffect` → target unchanged, staged temp deleted,
  `MutationRolledBack` journaled; a landed rename whose journal record
  fails is reported `AppliedButDurabilityFailed`, never "nothing
  happened";
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
