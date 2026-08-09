# Prototype Roadmap

The roadmap is ordered to validate the context hypothesis before expanding agent surface area.

## P0 — Architecture skeleton (this scaffold)

### Goal

Establish stable boundaries and a runnable event-driven prototype.

### Included

- Rust workspace and contracts;
- thin Agent Kernel;
- replaceable `ContextEngine`;
- non-vector `SimpleContextEngine`;
- continuous maintenance triggers;
- bounded tool outputs and artifact store;
- file list/read/write and shell execution contracts;
- policy approval gate (read-only default);
- JSONL runtime event journal;
- minimal TUI showing context-state counts;
- mock model provider.

### Acceptance

- kernel compiles without importing `context-simple`;
- tool runtime has no dependency on a context/memory crate;
- context state changes can be observed while a session runs;
- raw shell output does not enter model context unbounded;
- task completion archives task details and retains a summary.

### Risk

The scoring policy is deliberately naive; apparent quality at this phase is not evidence that the ranking design is final.

---

## P0.5 — Observability and replay ✅ (implemented)

### Goal

Make context behavior explainable before making it smarter.

### Status

- per-item lifecycle events: `ContextMaintained` reports carry
  `ContextStateTransition`s (from/to/reason/turn); `ContextPrepared` carries
  selected items with score components (`ScoreBreakdown`);
- score components, selection reason, eviction reason: recorded;
- turn ids / tool-round ids: assigned by the engine, exposed via
  `ContextDiagnostics` and stamped on items/transitions;
- deterministic replay: `crates/agent-replay` (see §11.2 of
  `docs/CONTEXT_LIFECYCLE.md`);
- export run traces as JSONL: unchanged (`agent-storage`);
- `context inspect` TUI panel (Tab): selected + transitions, event-driven;
- checkpoint runtime state separately from event history:
  `ContextEngine::checkpoint`/`restore` + `/checkpoint` command.

### Work

- add per-item lifecycle events;
- record score components, selection reason, eviction reason;
- assign turn ids/tool-round ids;
- add deterministic replay from event journal;
- export run traces as JSONL;
- add `context inspect` TUI panel with selected/evicted items;
- checkpoint runtime state separately from event history.

### Acceptance

Given a run, answer exactly:

- what entered context;
- why it entered;
- when it left;
- why it left;
- which model turns consumed it;
- whether it was later reactivated.

### Main risk

Instrumentation can accidentally become the hot path. Keep event payloads bounded and move heavy analysis offline.

---

## P1 — Real model provider + streaming ✅ (implemented)

### Goal

Run real coding tasks without coupling the kernel to one vendor.

### Status

- one provider adapter: `provider-openai` (OpenAI-compatible SSE; also covers
  DeepSeek/Qwen/Moonshot/GLM via `base_url`);
- streaming model output/events: `complete_stream` + `ModelEventSink` +
  `ModelChunk`; live `RuntimeEvent::ModelDelta` (not journaled);
- cancellation: `CancellationToken` on `ModelRequest`, `cancel_current_turn()`,
  clean turn end on cancel;
- provider capability declaration: `ModelCapabilities`;
- tool-call continuation mapping: streamed `ToolCallDelta` accumulation into
  `ToolCall`; malformed arguments degrade to `null`;
- usage accounting: parsed from the stream (`ModelUsage`);
- retry/backoff at transport boundary: `RetryingTransport` wrapper (retryable
  network/5xx/429 errors only, exponential backoff).

### Acceptance

- provider can be replaced without changing kernel/context/tools: satisfied —
  the kernel sees only the `ModelTransport` contract (see the P1 kernel tests
  in `crates/agent-kernel/tests/streaming.rs`);
- cancel stops model/tool continuation cleanly: satisfied;
- context diagnostics remain correct during streaming: satisfied — the kernel
  loop and maintenance triggers are unchanged; deltas bypass the journal.

### Work

- implement one provider adapter first;
- streaming model output/events;
- cancellation;
- provider capability declaration;
- tool-call continuation mapping;
- usage accounting;
- retry/backoff at transport boundary.

### Acceptance

- provider can be replaced without changing kernel/context/tools;
- cancel stops model/tool continuation cleanly;
- context diagnostics remain correct during streaming.

### Main risk

Provider-specific continuation semantics can leak into the kernel. Keep those inside the adapter or introduce a narrow continuation contract only when evidence requires it.

---

## P2 — Coding tool runtime ✅ (implemented)

### Goal

Become useful for real repository work while preserving context isolation.

### Status

- range/search tool: `search.grep` (regex, `rg`-style) — ignores
  `.git`/`.focus-agent`/`target`/`node_modules`/`vendor`/`dist`/`build`
  directories, bounded files scanned (5000) and per-file bytes (2 MB), 100
  model hits by default; larger hit sets go to an artifact;
- patch/edit tool: `edit.replace` — exact old→new with optional
  `occurrence`/`replace_all`; requires an unambiguous single match otherwise;
  no-op edits are rejected; full old content captured to the change journal
  when the file is small enough (256 KB);
- git tools: `git.status` (`--short`) and `git.diff` (`--staged`, optional
  path); read-only, 20 s timeout, tail bounded to 12 K chars + artifact;
- process tool: `shell.exec` — streaming with two reader tasks into a bounded
  channel (512), ring-buffer tail (200 lines) for the model, incremental
  artifact append for the full log, line cap (4000 chars) and output bound
  (12 K chars), kill-on-drop, per-call `timeout_ms` (≤ 120 s) and cooperative
  cancellation via `CancellationToken`;
- command cancellation: `ToolExecutionRequest.cancel` threaded into every tool
  (`search.grep`, `edit.replace`, `git.*`, `shell.exec`);
- workspace change journal: every mutating tool records a
  `WorkspaceChange` (tool/path/action/byte sizes/old content when bounded) to
  `.focus-agent/changes.jsonl`;
- interactive approval flow: `ApprovalBroker` (broadcast hub, pending queue)
  + `InteractiveApprovalGate` (kernel side) — read-only tools auto-allow,
  workspace-write/process-execution tools prompt the UI (tool name + args
  preview) with y/n/Enter/Esc; unanswered prompts time out (default 5 min,
  configurable) and deny instead of hanging the turn; `--read-only` CLI flag
  falls back to `PolicyApprovalGate::read_only()`;
- ignore rules for build artifacts/vendor directories: baked into
  `search.grep`.

### Acceptance

- medium repository can be inspected without loading a full tree or full
  files: satisfied — `search.grep` walks bounded files with per-file byte
  caps and ignores generated/vendor directories;
- large build/test logs remain artifacts: satisfied — `shell.exec` streams
  to an artifact and hands the model a bounded tail; `git.diff`/`git.status`
  spill to artifacts when truncated;
- model sees only relevant bounded slices: satisfied — every tool defines an
  explicit model-content bound (`MODEL_OUTPUT_CHARS`, `MODEL_HITS`,
  ring buffer);
- mutating actions are visible and reversible/reviewable: satisfied —
  `edit.replace` records old content in the change journal; write/process
  calls require explicit UI approval (or `--read-only`).

### Work

- range/search tool (`rg`-style);
- patch/edit tool instead of whole-file writes;
- git diff/status;
- process streaming with bounded ring buffer;
- command cancellation;
- workspace change journal;
- interactive approval flow;
- ignore rules for build artifacts/vendor directories;
- message-scoped attachments + explicit Pin.

### Main risk

Tool convenience can reintroduce context pollution. Every new tool must
define an explicit model-output budget and artifact policy. Follow-up work
can add a `changes.jsonl`-backed undo/revert tool and message-scoped
attachments; both stay behind the same approval + bounded-output rules.

---

## P3 — Context lifecycle experiments ✅ (implemented)

### Goal

Compare dynamic working-set behavior against append-only baselines.

### Status

- two baseline engines implementing the same `ContextEngine` contract as
  `SimpleContextEngine`: `context-baselines::AppendOnlyEngine` (A) and
  `RollingSummaryEngine` (B, append + collapse-oldest-into-summary once a
  token threshold is crossed, keeping a verbatim recency window);
- seven deterministic scenarios (roadmap list below) synthesized in
  `agent-replay::scenarios`; generic `run_engine` replays any trace through
  any engine and collects token-cost metrics (total/max input tokens,
  over-budget snapshots, lifecycle churn, final working-set size);
- CLI: `agent-replay --compare [scenario]` prints the A/B/C table
  (see `docs/EXPERIMENTS.md` for full results);
- the TUI can run any policy live: `--context=append|rolling|dynamic`
  (composition-root change only — kernel/tools/UI untouched);
- metric conventions shared across engines (same token estimator).

### Results (budget 12 K, see `docs/EXPERIMENTS.md`)

- C costs 8–14× less model input than A on heavy scenarios and never
  exceeds the budget; A blows past it (13–22 over-budget snapshots).
- C archives completed/old-task detail (final active 6–24 vs A/B 31–97):
  task-switch and post-completion contamination is the sharpest
  differentiator.
- B bounds peak tokens by collapsing history but loses task-relevant
  records outside its recency window, and still pays ~9× C's cost.
- Honest negative: on `superseded_decisions` C costs more — supersession is
  not yet modeled (that is P4 work), confirming the policy needs explicit
  supersession handling.

### Baselines

A. full conversation until model limit;

B. append + periodic summary at threshold;

C. dynamic working set (this design).

### Metrics

- input tokens per successful task;
- completion quality;
- regression/repeated mistake rate;
- stale instruction leakage;
- tool rounds;
- time to recover old information;
- context churn rate;
- incorrect eviction rate;
- task-switch contamination.

### Test scenarios

1. long refactor with changing files;
2. repeated test/fix loops with large logs;
3. explicit task switch and return;
4. contradictory/superseded design decisions;
5. high-volume irrelevant tool output;
6. completed task followed by unrelated task;
7. pinned constraint across many turns.

### Acceptance

Dynamic context must show a measurable advantage on at least token cost or
long-task focus without a material regression in task success.

Token-cost and task-switch focus advantages are demonstrated (see
`docs/EXPERIMENTS.md`). Completion-quality and repeated-mistake metrics
require a real model provider and are tracked as the next step; the harness
already produces the per-turn inputs a live measurement would need.

---

## P4 — Smarter non-vector context policy ✅ (implemented: experiment-driven slice)

### Goal

Improve selection without hiding behavior behind embeddings/ML.

### Status

Implemented the five experiment-driven rules (the P3 `superseded_decisions`
regression was the trigger). Everything remains explicit, keyword/entity
based and fully explainable through lifecycle transitions:

- **supersession/contradiction relationships**: decision-classified user
  messages (keyword-based) are tagged and promoted; a later decision sharing
  an entity with an earlier one archives it with reason
  `superseded by decision at turn N` and permanently excludes it from model
  requests;
- **error -> fix -> verified transition**: failed observations persist as
  `Working`; a new failure on the same entities supersedes the previous
  error (one live error per failure site), a successful result verifies the
  fix and archives it (`error verified fixed by successful tool result`);
  successful observations remain ephemeral;
- **decision and constraint promotion**: decisions get `decision` tag +
  importance 0.72; pins keep `Pinned` retention;
- **entity affinity**: a bounded hot-entity set is seeded by the last user
  message and extended by entities touched in tool observations (reset on
  user message / focus change, cap 24). Scoring gains an `entity_affinity`
  component (0.18 × the fraction of an item's entities in the hot set),
  exposed in `ScoreBreakdown` and the selection reason;
- **explicit dependency graph**: at ingest, a new item records up to 8
  dependencies on prior non-dropped items sharing at least one entity;
  `materialize` expands the working set with dependencies of selected
  items (best first, cap +8, a 1 K-token reserve carved out of the model
  budget; Dropped / superseded / verified-fixed items never re-enter;
  Archived dependencies only when they still clear the active threshold),
  each with reason `included as dependency of item <id>`. Edges surface in
  `ContextItemSummary.dependencies` and the replay report;
- **policy configuration/replay comparison**: all four rules are
  configurable (`SimpleContextConfig { supersession, error_verification,
  entity_affinity, dependency_expansion }`, default on) and `baseline_v0()`
  turns every one off to reproduce the P3-era policy; the P3 harness measures
  the delta (`superseded_decisions` input tokens 10.7 K -> 9.2 K with churn
  60 -> 40, `long_refactor` +0.9 K for dependency traceability, see
  `docs/EXPERIMENTS.md` §6).

Not implemented (roadmap remainder): structured task phase, automatic
task-boundary detection, LLM-generated task summaries with validation.

### Work

- structured task phase;
- explicit dependency graph between context items ✅;
- supersession/contradiction relationships ✅;
- file/symbol/entity affinity ✅;
- error -> fix -> verified transition ✅;
- decision and constraint promotion ✅;
- automatic task-boundary detection;
- LLM-generated task summaries with validation;
- policy configuration/replay comparison ✅.

### Acceptance

Selection decisions remain fully explainable and replayable: every
supersession, verification, recurrence and dependency edge is observable —
supersessions/verifications/recurrences as `ContextStateTransition`s with a
reason, dependency edges in `ContextItemSummary.dependencies` and the replay
report (`depends on: <id>`), affinity in the score breakdown and selection
reason. All visible in the replay report and TUI transitions panel.

---

## P5 — ContextCore adapter ✅ (implemented: process-boundary adapter)

### Goal

Replace `SimpleContextEngine` with ContextCore without changing the agent runtime.

### Status

The ContextCore integration shape is implemented as a real process boundary:

- `context-contextcore` — an adapter crate implementing the exact
  `ContextEngine` contract over a JSON-lines stdio protocol (`wire`: one
  request per line, one response per line; `ping` handshake, `ingest`,
  `maintain`, `materialize`, `diagnostics`, `inspect`, `checkpoint`,
  `restore`, `shutdown`). `ContextServiceAdapter` spawns the service,
  handshakes, and maps every trait call to a request/response;
- `agent-context-service` — a standalone service process that runs a real
  in-process engine (`--engine dynamic|append|rolling`) and speaks the same
  protocol;
- composition root: `agent-tui --context=service` runs the whole agent
  against the service engine. Kernel, tools, provider and UI are untouched.

### Acceptance

A composition-root change selects ContextCore. No tool, TUI, provider, or
core agent-loop code needs architectural rewrites: satisfied — the P5 test
`adapter_plugs_into_a_real_kernel_without_rewrites` runs a real
`AgentKernel` with the adapter as its context engine; the TUI selects it
with one CLI flag. A future real ContextCore runtime only has to speak the
wire protocol; nothing on the agent side changes.

### Main risk

ContextCore may expose richer concepts than the minimal trait. Extend
contracts only for demonstrated runtime requirements; do not leak the entire
ContextCore internal model through the Agent API.

The adapter deliberately keeps the wire protocol to the trait's own types —
no ContextCore vocabulary leaks through the Agent API.

---

## V1 — Turn Runtime and typed runtime framework

### Goal

Split the three responsibilities the prototype currently mixes: the execution
protocol (runtime-owned turn stack), the long-term working set (context
engine), and the composition of modules (a runtime framework). This follows
the review that identified: model input is rebuilt from the materialized
working set, but nothing distinguishes the current turn's tool protocol from
long-term memory; the context engine has grown too dense; the kernel
orchestrates via `Mutex`es while the TUI spawns tasks that mutate shared
state.

### V1-M1: Turn Runtime ✅ (implemented)

The model input is now assembled in five layers:

```text
System Policy    - standing instructions (runtime-owned)
Focus Frame      - the current task/goal (structured in a later phase)
Context Frame    - the selected working set from ContextEngine::materialize
Turn Frame       - the current turn's execution stack (runtime-owned)
Active Tool Schemas - tool definitions for this request
```

Implemented:

- `ModelMessage` gained `tool_calls` (assistant) and `tool_call_id` (tool
  result pairing), so a turn renders as standard protocol messages instead of
  text inside a system block;
- `TurnFrame` — the runtime-owned execution stack (user message, assistant
  tool calls, tool results, in order). It is never scored, garbage-collected
  or evicted while the turn is open; it is dropped when the turn ends;
- `ModelInput::into_messages()` flattens the five layers in protocol order
  (policy, focus, context, then user -> assistant tool calls -> tool results);
- the kernel turn loop keeps the `TurnFrame` and no longer ingests tool
  results during the turn. When the model finishes, the turn's observations
  are persisted at once (`ingest(ToolObservation)` x N + one
  `maintain(AfterTool)`), then the final assistant message;
- the OpenAI-compatible provider serializes assistant tool calls and
  `tool_call_id`-paired tool results on the wire.

Acceptance:

- during a turn, the model sees tool results as protocol-paired messages
  (`tool_call_id` matches the assistant call), and the context engine sees no
  observation until the turn ends — covered by
  `agent-kernel/tests/turn_frame.rs` and the contract/provider tests;
- replay and the A/B/C comparison are unaffected (they drive the engines
  directly, not the kernel);
- `ContextEngine` contract unchanged: all four implementations, the process
  boundary, the TUI and the replay harness still build.

### V1-M2: Context runtime modules + Scope ✅ (implemented)

- `context-simple` is one crate with modules, mechanism and policy
  separated without splitting into dozens of crates: `engine` (config +
  state + ingest dispatch), `item`, `heap`, `scope`, `residency`,
  `index/` (`entity`, `task`, `dependency`, `indexes`), `policy/`
  (`simple`), `gc/` (`minor`, `reachability`), `materializer`,
  `diagnostics`, `checkpoint`. Since V1-M9 the slot-based secondary
  indexes (`index/indexes.rs`: id→slot, entity→ids, scope→ids) back
  dependency ingest and materializer candidate generation — the
  id→index map is no longer a later optimization, it is the hot path.
- the `Scope` entity is first-class in the contract: `ScopeId`,
  `ScopeKind` (Session / Task / Focus / Tool), `ScopeState` (Open /
  Active / Suspended / Closed) and `Scope` (id, parent, kind, state,
  task_id, goal, opened / last-active / closed ticks). The engine keeps
  a scope tree in `State` and drives it from ingest: session opens
  lazily, task/focus open per task (old task suspends on a task
  switch), one tool scope per tool call closes at `AfterModel`.
- closing a task scope (on `TaskCompleted`) promotes its durable
  outcomes — decisions, findings, constraints, open loops, artifact and
  evidence references, pinned/durable items — to the session scope and
  evicts the rest of the working set with an explainable `task
  completed: scope closed, working set evicted` transition; promoted
  findings can later reactivate when a related task touches the same
  entities;
- `ContextDiagnostics` exposes scope counts (open/active/suspended/
  closed); checkpoints round-trip the scope tree.

Acceptance:

- all workspace tests green (74), including new ones: scope tree
  layout, task-switch suspension, tool-scope close at `AfterModel`,
  task close promote/evict, promoted-finding reactivation, checkpoint
  scope round-trip;
- behavior is otherwise preserved: item residency, supersession, error
  verification and dependency expansion tests are unchanged and green;
- `ContextEngine` contract unchanged (diagnostics gained serde-default
  scope fields only); replay/A/B/C harnesses unaffected.

### V1-M3: Runtime framework ✅ (implemented)

- new crate `agent-runtime`: `RuntimeHandle` -> `mpsc<RuntimeCommand>` ->
  `RuntimeActor` owning the mutable runtime state. Every command (user
  message, focus, pin, task completion, checkpoint, diagnostics, inspect,
  cancel, stop) is serialized by the actor, so focus/pin/task commands can
  no longer race an in-flight turn — the structural race where the TUI
  spawned kernel calls directly is gone (the TUI now drives the handle);
- long-running work (a turn) runs as a spawned operation reporting back an
  `OperationResult` tagged with run/turn/task/scope/operation ids and a
  generation. The actor drops results whose generation moved on (cancel,
  stop) instead of letting them race into the new state — observable as a
  `stale turn result dropped` warning;
- the actor stays responsive while a turn runs (it selects on both the
  command channel and the completion channel), so `/cancel` is processed
  mid-turn and a new turn can start immediately after;
- `ModuleHost` with a uniform lifecycle — `add_module` (register +
  capability-claim validation), `start` (in order), `stop` (in reverse)
  — over a typed `ServiceRegistry`: modules publish typed capabilities
  (`ContextService`, `ModelProvider`, `ToolProvider`, `ApprovalPolicy`,
  `EventStore`, `ArtifactStore`, all `CapabilityProvider` markers in
  `agent-contracts`), consumers look them up by type. There is no
  universal `handle_event`;
- the TUI composes the run through the host (context/model/tools/approval/
  events/artifacts modules) and reads the capabilities back into the
  kernel; the actor owns start/stop.

Acceptance:

- workspace tests green (79): `agent-runtime` adds actor tests (busy
  rejection, cancel + stale-result drop, clean stop) and host tests
  (typed lookup, duplicate-claim rejection, lifecycle order, missing
  capability);
- the kernel contract is unchanged for replay/A/B/C (they drive engines
  directly); `set_focus` now returns the new `TaskId` so the runtime can
  tag operations with their task.

---

### V1-M6: Context GC v1 ✅ (implemented)

The GC dimension is separated from the semantic state machine instead of
being folded into `ContextState`:

- `ContextResidency::Resident | Evicted` — where the item physically lives
  (heap vs bounded eviction buffer), a new field on `ContextItem` together
  with `gc_generation: u32` (full passes survived without being a root) and
  `evicted_at_tick: Option<u64>` (stamp that keeps same-pass bounce-back
  out of the reactivation scan);
- `ContextEngine::gc()` — a full mark/sweep/reactivate pass with a default
  no-op implementation, so baselines and the wire adapter keep working
  unchanged; `AgentKernel::context_gc()` forwards it and the `RuntimeActor`
  runs it at turn boundaries, emitting `RuntimeEvent::ContextGc`;
- mark phase: roots = pins, active task/focus scope members, durable
  session memory, hot-entity matches, bounded dependency reachability;
- sweep: semantically Dropped items are evicted unconditionally (they no
  longer linger in heap/checkpoint forever), unmarked live items climb
  `gc_generation` and are evicted past `gc_max_generation` (3) or the
  `turn_ttl_ticks * 4` age bound; Active items are never evicted — GC
  compacts what the policy demoted, it does not fight it;
- reversible eviction: a bounded buffer (`gc_buffer_capacity`, 256)
  reactivates items whose entities are hot again, that are pinned again,
  or whose score still clears the active threshold
  (`gc_reactivate_per_pass`, 8); overflow purges the oldest, counted;
- explainability: `ContextGcReport` carries per-item `ContextEviction` /
  `ContextReactivation` records with human-readable reasons, and
  `ContextDiagnostics` gained resident/evicted counts plus cumulative
  gc counters; the replay harness drives `engine.gc()` for `ContextGc`
  events in a trace.

Acceptance: 5 new `gc::full` tests (dropped-item eviction with reason,
root marking vs generational eviction, hot-again reactivation with reason,
bounded buffer + purge, generational ladder to the cap); full workspace
build/test/clippy/fmt green; A/B/C replay baseline unchanged.

---

## V1-P0: structural fixes — the actor owns the runtime

Follow-up review of V1-M3. Each item is a small milestone with its own
verification; they are being implemented in order.

### V1-P0-1: the actor is the runtime ✅ (implemented)

`AgentKernel` no longer runs turns. The turn execution state machine lives
in the `RuntimeActor`:

```text
RuntimeActor
   ├─ prepare model operation (maintain BeforeModel, snapshot, model input)
   │       ↓
   │    async execute            -> OperationResult
   │       ↓
   ├─ validate generation        (stale => drop + warning, nothing committed)
   ├─ commit                     (turn-frame push / context ingest / events)
   │
   ├─ prepare tool operation (ToolStarted)
   │       ↓
   │    async execute            -> OperationResult (approval + dispatch)
   │       ↓
   ├─ validate generation
   ├─ commit                     (tool result into the turn frame, ToolFinished)
   │
   └─ continue state machine     (next tool / next model round / finalize)
```

Consequences:

- model rounds and tool calls are real `Operation`s — `OperationOutcome::
  ModelOutput` and `::ToolOutput` are now used, not just `Completed`;
- only the actor commits: a stale result (cancel, generation bump) is
  dropped before it can touch the context, the turn frame or the event
  stream; the previously unavoidable side effects of "the whole kernel
  turn already finished" are gone;
- `AgentKernel` is a stateless executor/helper (context/model/tool
  primitives + event plumbing + journal); its `turn_lock`/`turn_cancel`
  and the `TurnFrame` ownership are gone — execution ownership is no
  longer duplicated;
- the actor owns the `TurnFrame` (execution stack) across model/tool
  operations, so the M1 invariant (turn frame is never scored or evicted
  mid-turn) is enforced by construction;
- cancellation is committed by the actor immediately (Warning + TurnCompleted),
  and the in-flight operation's late result is dropped as stale.

Acceptance:

- workspace tests green; the kernel streaming/turn-frame/approval tests
  moved to `agent-runtime` (they now test the actor): streamed deltas,
  cancellation of a hanging model round, turn-frame protocol pairing +
  persist order, and the interactive approval loop (allow / deny /
  timeout) all pass against the new state machine.

### V1-P0-2: prompt assembly leaves the context engine ✅ (implemented)

The context engine no longer knows the prompt protocol:

- `ContextBuildRequest { system_prompt, current_input, budget }` and
  `ContextSnapshot { messages }` are gone. The contract is now
  `ContextQuery { current_input, budget_tokens, hints }` ->
  `MaterializedContext { focus, items, selected, approx_tokens,
  diagnostics }`, where `items` are structured `MaterializedItem`s
  (content, kind, scope, state) — never rendered `ModelMessage`s;
- a single `PromptAssembler` in `agent-runtime` is the only place that
  renders System + Focus + Context + Turn + Tool Schemas into the
  five-layer `ModelInput`. The actor subtracts the system prompt's own
  token cost before handing the budget to the engine;
- `ContextHints { max_selected_items }` opens a per-request knob the
  engine honors (primary selection and dependency expansion both cap);
- the token estimator moved to `agent-contracts::tokens` so engines,
  the assembler and the replay harness measure the same quantity;
  replay now reports the true input cost (system prompt + materialized
  share) instead of an engine-rendered approximation;
- the wire protocol carries the same change (`build_snapshot` ->
  `materialize`), so the ContextCore adapter path is unchanged in shape.

Acceptance:

- workspace tests green (including a new `max_selected_items` cap test
  and the context-service end-to-end tests across the process boundary);
- the engine returns `Vec<MaterializedItem>` and the kernel/actor never
  reassemble prompt text from engine output.

### V1-P0-3: scope ownership moves to the runtime ✅ (implemented)

Scope membership is now authoritative and tool scopes are execution frames:

- `ContextItem.scope_id` (and `ContextItemSummary.scope_id`) stamp every item
  with the scope it was produced in; `close_members` decides membership by
  walking the `scope_id` tree (task and focus closes see focus descendants,
  tool frames stay out — their observations leave through residency and
  error verification), with a legacy fallback for items without a stamp
  (restored old checkpoints);
- tool scopes are runtime-driven: the actor opens a fresh tool scope at
  tool start (`ContextEngine::open_scope`) and closes it when the next
  model round consumes the result (`ContextEngine::close_scope`); the
  observation persisted at turn end carries the tool scope id, so the
  membership is authoritative even though persistence is batched;
- `close_members` now processes tool scopes: durable members promote to the
  parent, ephemeral/working results are left to residency and verification
  (the ingest-time `open_tool_scope` and the `AfterModel` tool-close queue
  are gone);
- session/task/focus scopes stay focus-state-derived (opened by the
  ingest the runtime already issues on `Start` / `set_focus`), and task
  close keeps flowing through `maintain(TaskCompleted)` so the promotion
  transitions stay observable;
- the wire protocol carries `open_scope` / `close_scope`, so the
  ContextCore service path drives the same frame lifecycle; the baselines
  accept scope ids as no-ops (they retain no scope tree).

Acceptance:

- workspace tests green, including a new actor-level test asserting the
  tool scope opens at tool start and closes with the same id when the model
  consumes the result, with the observation tagged by that id;
- engine tests assert membership by `scope_id` (focus-scoped working set
  archived through the task close) and that a consumed tool scope returns
  the active pointer to its parent.

### V1-P0-4: the module host becomes an extension platform ✅ (implemented)

Two extension planes, one stable core ABI:

- **Trusted core services stay typed.** `ServiceRegistry::register` and
  `get` are public, so modules from external crates can publish and
  retrieve typed capabilities — the core ids (`context-service`,
  `model-provider`, `tool-provider`, `approval-policy`, `event-store`,
  `artifact-store`) are no longer the only things that can live in the
  registry;
- **Dynamic capability platform.** `agent-contracts` defines
  `CapabilityManifest { id, version, name, summary, permissions,
  dependencies, lifecycle, transport }` and the `Capability` trait. The
  chain is `Service -> Capability -> Tool Schema -> LLM`: a registered
  capability advertises tool schemas that join the runtime's tool
  provider, and model calls route back to `invoke`:
  - `CapabilityRegistry` (shared, runtime-mutable) accepts registrations
    before start *and* mid-run — an LLM (or any external actor) can
    publish new capabilities while the runtime is running;
  - `CapabilityAwareDispatcher` merges the built-in tools with the
    capabilities' tools, so the kernel keeps talking to one
    `ToolDispatcher`;
  - lifecycle is honored (`Eager` starts with the host, `Lazy` starts on
    first invocation); declared dependencies are validated at
    registration; permissions stay declarative — the advertised tools
    carry `ToolRisk` levels the approval gate enforces;
  - process transport is declared in the manifest but not yet resolved
    (the prototype implements `InProcess`).
- the TUI composition root wires the tool provider through the
  `CapabilityAwareDispatcher`, so a capability registered at runtime is
  callable by the model on the next request.

Acceptance:

- workspace tests green, including: an external-crate-shaped module
  publishing a typed service through the public path; a capability whose
  tool reaches the tool provider and routes calls; mid-run registration
  with lazy start-on-first-use; dependency and duplicate-id validation.

### V1-P0-5: workspace confinement is a Trusted Core boundary ✅ (implemented)

`Workspace::resolve_relative` is now a confinement check, not a lexical
join:

- lexical guards stay (`..`, absolute paths, drive prefixes, root
  components are rejected);
- the candidate is then walked component by component from the canonical
  root: every existing intermediate is canonicalized and must still be
  under the root, so symlinks, junctions and reparse points anywhere
  along the path cannot redirect the tail outside the workspace;
- missing tail components are appended lexically afterwards, so
  new-file writes keep working;
- `resolve_mutation` adds a hard rejection of the runtime state
  directory (`.focus-agent/` — traces, checkpoints, artifacts, change
  journal), and mutating tools (`fs.write`, `edit.replace`) resolve
  through it. Reads (`fs.list`/`fs.read`/`search.grep`) keep
  `resolve_relative`.

Acceptance: workspace tests green, including real junction escapes on
Windows (`mklink /J`) being rejected, state-dir writes rejected through
both a direct path and a link into the state dir, and normal relative
resolution unchanged.

### V1-P0-6: mutations become journaled transactions ✅ (implemented)

`Workspace::begin_mutation` returns a `MutationTransaction` replacing
per-tool file writes:

```text
resolve_mutation → capture old content (bounded) → stage temp file in
target dir → record change journal → atomic rename (swap)
```

- the journal entry is written *before* the swap, so a journal failure
  never leaves the target half-mutated and a retrying agent cannot
  double-apply an already-landed mutation; any failure removes the temp
  file and leaves the target untouched;
- `edit.replace` and `fs.write` both go through the transaction —
  `fs.write` now journals every write (action `write`, old content
  captured up to `CHANGE_CAPTURE_LIMIT`) instead of bypassing the change
  journal;
- the temp file lives next to the target (same filesystem) so the swap
  is a true atomic rename.

Acceptance: workspace + tool tests green; `fs.write` journals new-file
and overwrite cases with old content; a failed commit (rename over a
non-empty directory) leaves unrelated targets untouched and cleans up
the temp file.

### V1-P0-7: the context budget becomes a real model budget ✅ (implemented)

The engine's `budget_tokens` is now the *residual* of a full request
budget computed at the runtime top level (`agent-runtime::budget`):

```text
Provider Context Window (ModelCapabilities.context_window, falls back to
                         the kernel's configured budget)
        - Output Reserve        (max_output_tokens, or a default reserve)
        - System Policy         (the assembled system prompt)
        - Turn Frame            (wire-form token estimate of the turn stack)
        - Active Tool Schemas   (wire-form estimate of the tool specs)
        = Context Frame Budget  (the only number the engine receives)
```

- `ModelCapabilities` gains `context_window: Option<usize>` (providers
  leave it `None` until they declare one); the kernel exposes
  `model_capabilities()`;
- the engine stops deducting `current_input` from its budget — the
  input rides in the runtime's turn frame and is charged there, so it is
  never double-counted; focus remains engine-owned;
- pinned items now get priority without exemption: selection is a
  two-pass loop (pinned first, scored rest) and every item — pinned or
  not — must fit the remaining budget, so the frame is a hard bound.

Acceptance: budget arithmetic unit tests; an actor-level test records
the `ContextQuery` and asserts the engine received exactly
`window - output - system - turn - tools`; a materializer test pins a
tiny and a huge item and asserts the huge one cannot blow the budget
while the tiny one is always selected first.

### V1-P0-8: shutdown and durability ownership move into the runtime ✅ (implemented)

Shutdown is no longer a best-effort sequence owned by the caller:

- the actor's `Stop` arm runs a real `shutdown()` (cancel the turn, then
  `kernel.stop()` — journal flush + `RunCompleted`) and **replies with
  the kernel stop result** instead of swallowing it;
- the `rx.recv() -> None` path (every caller handle dropped) runs the
  same teardown instead of returning silently, so durability work never
  depends on the caller remembering to stop;
- a new `RuntimeInstance` (`agent-runtime`) owns the `ModuleHost`, the
  `RuntimeHandle` and the actor `JoinHandle` together, with one ordered
  `shutdown()`:

```text
cancel any turn
  → stop the actor (kernel stop: flush journal, emit RunCompleted)
  → stop the module host (reverse registration order)
  → join the actor task
  → aggregate errors into one result
```

- the TUI composition root now uses `runtime.shutdown().await`; a
  shutdown failure (e.g. a journal flush error) surfaces instead of
  being discarded, and the actor join handle is no longer thrown away.

Acceptance: an actor test drops every handle (keeping only a broadcast
subscriber) and asserts `RunCompleted` still fires; `RuntimeInstance`
tests assert the module lifecycle brackets the run (`start` ... `stop`),
that `shutdown` aggregates a failing module stop into its error, and
that shutdown never hangs when the actor was never started.

## V1-P5: string tags become typed labels ✅ (implemented)

Scope promotion and GC decide membership by enum values, not string
matching (`agent-contracts::label`):

- `CoreLabel` — the six content labels (decision, finding, constraint,
  open-loop, artifact-ref, evidence-ref);
- `LifecycleLabel` — the markers the GC/promotion machinery stamps
  (promoted, superseded, verified-fixed);
- `Label::Extension` — namespaced labels (`ext:github/pr`) for modules
  that need their own vocabulary.

`ContextItem.tags` is now `Vec<Label>`; labels serialize as their string
form and accept any string back, so old checkpoints and future labels
round-trip. A misspelled tag can no longer silently change lifecycle
behavior.

## V1-P6: builtin tools get a real lifecycle ✅ (implemented)

`BuiltinToolDispatcher` becomes a catalog with an active set:

```text
ToolCatalog → capability discovery → ActiveToolSet → model request
```

- tools move through `Available → Loaded → Active → Warm → Unloaded`;
  only loaded/active tools appear in `specs()`, so the model sees the
  lean set, not all eight;
- a lazy GC cools idle tools out of the surface (`Loaded → Warm →
  Unloaded`) on every model request; core tools (`fs.list`, `fs.read`,
  `search.grep`) never age out and cannot be unloaded;
- the always-visible control tools `capability.search` /
  `capability.load` / `capability.unload` let the model discover and
  drive the lifecycle itself (load `git.status` when the task needs
  git). Execution of a catalog tool is always permitted; the lifecycle
  gates the model surface and the approval gate protects side effects.

Acceptance: tests assert the default surface is the core set plus the
control tools, load/unload changes the surface (core unload rejected),
idle tools cool and unload under aggressive GC config, and the control
tools execute end to end.

## V1-P7: capability maturity is a ladder, not a declaration ✅ (implemented)

`CapabilityManifest.status` carries `Experimental → Tested → Validated →
Stable → Deprecated`. The registry pins every out-of-process registration
to `Experimental` regardless of its declared status — an LLM cannot
promote its own module — while trusted in-process capabilities keep their
declaration. The registry exposes the effective status and a catalog
snapshot for the discovery surface. The full self-improvement loop
(`capability.inspect/install/test/enable/disable`, with the
replay/evaluation infrastructure as the evaluator that climbs the
ladder) is future work on top of this.

## V1-P8: module host manifest restructure + process transport hardening ✅ (implemented)

- `CapabilityTransport` is `Builtin | Process` (`Wasm` documented as
  future). The trusted core stays in-process; the first version never
  loads Rust plugins (the ABI is not a stable plugin boundary, and a
  crashed plugin must not take the runtime down). LLM/third-party
  extensions are out-of-process over a framed protocol.
- The manifest declares `provides: Vec<CapabilityKind>` (tool/skill/
  service) and `requires` (renamed from `dependencies`, still
  deserializes under the old name).
- The context-service process boundary is hardened into the reference
  plugin IPC: a versioned handshake (`PROTOCOL_VERSION` echoed on every
  response), a per-request deadline (`request_timeout`), and a frame-size
  bound (`max_frame_bytes`) so a wedged or broken service cannot hang a
  turn or grow the adapter's memory.

## V1-M9: Adaptive Runtime — context meta-tools ✅ (implemented)

The LLM can tune the runtime's working set, but cannot bypass the kernel:
permissions, budgets and approval stay kernel-owned. Four read-only context
meta-tools — `context.gc_hint` / `context.tag` / `context.lease` /
`context.collect` — return a `RuntimeDirective` the runtime routes to the
context engine; tools still never touch the engine (invariant 3).

- contract seam: `ToolOutcome::RuntimeDirective { output, directive }`
  (`agent-contracts`), with
  `RuntimeDirective::Context(ContextAction)` where
  `ContextAction::{GcHint{item_id, keep_alive}, Tag{item_id, tag},
  Lease{item_id, turns}, Collect}`. Producing a directive requires the
  `runtime:context-control` permission in the capability manifest; a tool
  without it is denied;
- engine application: `ContextIngress::ContextDirective { action }` —
  `gc_hint` sets `keep_alive`, `tag` pushes a deduped
  `Label::extension(tag)`, `lease` stamps `lease_until_turn = turn +
  min(turns, max_lease_turns)`. Directives search the heap *and* the
  eviction buffer, so a hint/lease on an already-evicted item brings it
  back on the next GC pass; a stale `item_id` is a silent no-op. Quotas
  bound the model's power to root the heap: `max_keep_alive_items`,
  `max_leased_items_per_task`, `max_leased_tokens_per_task`,
  `max_lease_turns` — a refusal surfaces as an `InvalidRequest` error, and
  keep_alive/lease auto-expire on task close;
- GC roots (`context-simple` `gc/full.rs`): `keep_alive` or a live lease
  marks an item `model_directed_root`, and an explicit directive overrides
  the consumed-ephemeral heuristic in the sweep — a spent turn observation
  the model asked to keep stays resident. Reactivations report
  `kept alive by a model gc_hint` / `leased by the model until turn N`;
- actor routing (operation-commit time, inside the generation fence that
  guards effect commit): each tool result executes its directive *before*
  the observation ingest — `Collect` runs a full `ContextEngine::gc()`
  immediately and emits `RuntimeEvent::ContextGc`, everything else becomes
  a `ContextDirective` ingest;
- the model can address items because the context frame exposes each item's
  id (`id=<...>` per frame line in the prompt);
- the meta-tools are always loaded with the core set
  (`fs.list`/`fs.read`/`search.grep` + the four `context.*` tools).

Acceptance: engine tests cover gc_hint keep/release on a consumed
observation, a hint reaching an evicted buffer item (reactivates on the
next GC), tag dedup, lease expiry, and the quotas (keep_alive cap refusal,
lease turn clamp, per-task count/token caps, task-close expiry); turn
tests cover collect → `ContextGc` event and a directive routed before the
observation ingest;
tool-runtime tests cover the four schemas and executions. Full workspace
build/test/clippy/fmt green; A/B/C replay baseline unchanged.

## V1-M9: process sandbox + external store retrieval loop ✅ (implemented)

Two review blockers cleared before V2 autonomous capability generation:

- **The process boundary is a sandbox, not a permission note.**
  `ProcessHost` runs every child inside `ProcessSandbox`: env whitelist
  (the process-capability adapter's profile is PATH/SystemRoot/
  SystemDrive/TEMP/TMP only — no inherited secrets, `OPENAI_API_KEY` and
  `HOME` never cross), a dedicated per-capability cwd, Unix rlimits
  (RLIMIT_CPU / RLIMIT_NPROC; 60 s / 16 processes), and
  `call_with_cancel` — a cancel (user `/cancel`, superseded operation)
  poisons the connection and kills the whole process tree immediately
  instead of waiting for the request deadline. V2 autonomous capability
  generation stays gated until filesystem isolation beyond the cwd and a
  network policy land.
- **The context store is a retrieval loop, not a black hole.** The store
  path is injected at the composition root
  (`workspace.state_dir()/context-store`); the leaked CWD-guessed copy was
  removed and `.gitignore` covers `**/.focus-agent/`.
  `context.search` / `context.inspect` / `context.fetch` (read-only,
  always loaded) resolve through `ContextEngine`'s new
  `search_external` / `inspect_external` / `fetch_external` —
  deterministic, no vectors. `MaterializedContext.external` is a bounded
  view (max 32 refs, quickselect-ranked by hot entity / open loop /
  recency), never a clone of the map; `ExternalizedContext` keeps the
  entity signature so Cold recall filters in memory before touching disk;
  external TTLs count GC generations (`gc_epoch`), not ticks; the full GC
  pass splits into plan (lock) -> store IO (no lock) -> commit (fresh
  lock); Storage GC is a reachability closure with `Result<DeleteOutcome>`
  so real IO errors keep entries; `ContextHeap` makes index consistency
  structural (a stale index is a type error, not a length guard).

## V1-M9: merged tool surface + hardened capability plane ✅ (implemented)

The always-visible schemas are context too, and the capability plane
stopped trusting declarations:

- **Merged meta-tools.** `context.manage` (op: gc_hint/tag/lease/collect/
  search/inspect/fetch) and `capability.manage` (op: search/inspect/load/
  unload) replace a dozen single-purpose control tools; the default model
  surface drops from fourteen schemas to five (fs.list / fs.read /
  search.grep / context.manage / capability.manage). The merge is
  measured, not assumed: a token benchmark asserts the merged
  always-visible surface costs decisively fewer schema tokens.
- **Bounded catalog.** `capability.manage op=search` pages (default 20,
  cap 50, name-sorted cursor) and spills the full listing to an artifact;
  registration caps tools per capability (32), tool-name length/character
  set, description length and per-schema bytes (4 KiB) — a single
  capability cannot blow up the model surface.
- **Unified surface generation.** The capability registry carries its own
  counter bumped on register/activation/load/unload; the dispatcher
  snapshot combines it with the builtin catalog's generation, so dynamic
  capability changes are auditably visible.
- **No callbacks under the registry lock.** Registration reads and
  validates the manifest + tool schemas once, then caches them; every
  catalog query reads the cache. A slow or re-entrant capability can only
  misbehave at register time.
- **Final budget guard is the input budget.** The assembled request must
  fit `context_window - output_reserve` (rendering overhead may not eat
  the answer's reserve); with the context frame emptied it auto-unloads
  optional tools, and an unshrinkable request is a hard error — never a
  silently over-budget send.
- **Selection respects scope state.** The materializer's candidate scopes
  are the session, the active task's open task/focus scopes and open tool
  frames — closed tool frames no longer re-enter the prompt by task
  membership alone (they come back via retention, affinity or dependency),
  matching the GC mark phase's closed-scope boundary.

## V1-M10 Runtime Consistency ✅ (implemented)

Three consistency gaps closed before V2:

- **Turn finalization is a commit.** `finalize_turn` walks `Running` →
  `ModelFinished` → `Committing` → `Committed`; every mandatory state write
  (observation ingest, `AfterTool`/`AfterModel` maintenance, GC, and their
  journal events) must succeed before `TurnCompleted` is emitted. On the
  first failure the commit aborts and the runtime journals
  `TurnCommitFailed { phase, message }` + `RecoveryRequired` — "the model
  answered" and "the runtime durably committed this turn" are two facts.
  This is the crash-recovery foundation.
- **Capability lifecycle is serialized.** Each registry entry carries a
  `CapabilityRunState` (`Stopped`/`Starting`/`Started`/`Stopping`/`Failed`)
  plus a per-capability async run lock held across `start()`/`stop()`:
  concurrent `ensure_started` calls collapse into one `start()`, a failed
  start is observably `Failed` and retryable, and stops cannot race starts.
- **Process host split out of the ContextCore adapter.** The generic host
  moved to `agent-process` (framed IPC, child lifecycle, sandbox hooks) and
  the process-capability adapter to `agent-capability-process`;
  `context-contextcore` is now only the ContextEngine adapter + wire.
- **Storage GC parity, enforced by test.** `ServiceOp::StorageGc` is on the
  wire, the adapter overrides `storage_gc`, and a full-contract parity test
  drives *every* `ContextEngine` method through both an in-process engine
  and the service boundary, asserting identical normalized outcomes. The
  checklist is one `contract_snapshot` helper; the service binary is a
  dev-dependency of the adapter crate so it is rebuilt whenever the
  protocol changes.

Performance P0 (store out of the CWD, sync disk IO out of the context
lock, bounded external view) and the consistency invariant test suite
(task id alignment, transactional task transitions on set/clear-focus
failure, checkpoint/restore alignment, stale-effect rollback, durability
failure reporting, directive-before-next-round, child termination on
cancel, store confinement, exact fetch recovery, full-contract parity,
window-vs-reserve budget) are in place; see `docs/ARCHITECTURE.md` §9e/§9f.
The milestone acceptance — the runtime and the context never drift into a
task/state split-brain — is covered end to end by the runtime tests: the
actor assigns the task id and the engine carries the same one
(`runtime_task_id_matches_the_context_task_id`), and checkpoint/restore
re-aligns both (`runtime_checkpoint_roundtrips_tasks_context_and_capabilities`).

Performance P1 landed ahead of the milestones below: the external map
owns its id/entity indexes (`ExternalMap`, O(1) inspect/fetch lookups,
index-accelerated recall), `MaterializedContext.external` is a
type-enforced `ContextMapView` (cap 32, wire-validated), the per-round
tool surface is bounded by a deterministic schema budget
(`MAX_TOOL_SURFACE_TOKENS`), and capability catalog rows are cached per
`catalog_version`; see `docs/ARCHITECTURE.md` §9g and
`docs/CONTEXT_LIFECYCLE.md` §9j.

Performance P2 closed out the q32 performance list: the scope tree owns
its id index (`ScopeTree`, O(1) close/ancestor lookups), the materializer
selects by top-K (quickselect trim under `max_selected_items` +
deterministic ordering, bounded max-heap for dependency expansion),
dependency edges are typed (`DependencyEdge` with a legacy bare-id wire
form), and the GC/storage IO phases batch their file operations on a
`JoinSet` so the lock-free window shrinks to the slowest single
operation; see `docs/ARCHITECTURE.md` §9h and
`docs/CONTEXT_LIFECYCLE.md` §9k.

## Next: V1-M10 → V2 (ordered)

1. **V1-M10 Runtime Consistency** — task authority, transactional task
   transitions, RuntimeCheckpoint, Turn commit. Acceptance: the runtime and
   the context never drift into a task/state split-brain. ✅ (implemented —
   see the section above and the consistency invariant suite in
   `agent-runtime/tests/instance.rs`)
2. **V1-M11 Context Recall** — store injection, `ContextMapView` (the
   type-level bounded view landed in Performance P1),
   `context.search`/`fetch`, `gc_epoch`, async store. Acceptance: external
   information can be pulled back on demand without polluting the prompt.
   ✅ (implemented — see the section below)
3. **V1-M12 Effect Runtime** — every capability routes side effects through
   one unified EffectRequest/Effect commit. Acceptance: a cancelled
   operation produces no avoidable stale mutation. ✅ (implemented — see the
   section below)
4. **V1-M13 Extension Sandbox** — process sandbox, env scrub, brokered
   FS/network, cancel. Acceptance: experimental code cannot exceed the
   permissions granted to it. ✅ (implemented — env scrub, sandboxed cwd,
   resource limits and cancellation; see the section below)
5. **V1-M14 Resource Policy** — tool schema budget (the per-round surface
   bound landed in Performance P1), context hint quota,
   RiskClass, PermissionSet. Acceptance: the LLM cannot exhaust runtime
   resources through meta-tools.
6. **V1-M15 Real Evaluation** — coding workload A/B/C + lifecycle metrics.
   Acceptance: the dynamic runtime saves tokens without lowering task
   success rate.
7. **V2 Self-Iteration** — generate → sandbox → test → replay → evaluate →
   canary → stable. The LLM grows capabilities, but cannot modify the
   evaluation or permission Core.

M12/M13 strictly precede Self-Iteration: a capability that can already
stage effects and a sandbox that can contain it are prerequisites for
letting the LLM grow capabilities autonomously.

## V1-M13 Extension Sandbox ✅ (implemented)

A process capability runs in a strict, static execution boundary
(`ProcessSandbox` in `agent-process`, built by
`ProcessCapabilityAdapter::from_manifest`):

- **Env scrub.** The child inherits *only* an explicit whitelist of
  non-secret platform essentials (PATH/SystemRoot/SystemDrive/TEMP/TMP);
  every other parent variable — API keys, HOME, credentials — is dropped.
  Explicit `env` grants land after the whitelist. Covered end to end:
  `agent-process/tests/sandbox.rs` (unlisted secret scrubbed, whitelisted
  variable inherited, explicit grant delivered) and the capability-level
  `strict_sandbox_scrubs_parent_secrets_across_the_wire`.
- **Sandboxed cwd.** The child runs in its own dedicated working
  directory, created at connect, never the parent's cwd — a generated
  capability cannot roam the workspace by relative paths. Covered by
  `sandbox_cwd_is_created_and_isolates_the_child`.
- **Resource limits (Unix).** Hard `RLIMIT_CPU` / `RLIMIT_NPROC` ceilings
  applied by the kernel right after fork. Note the `RLIMIT_NPROC` caveat:
  on Linux it is a *per-user* ceiling, so a small value (the default 16)
  can starve the child on a host where the same user already runs many
  threads (observed on the CI runner). The limits are implemented but not
  asserted in the sandbox acceptance tests, which focus on the env and cwd
  dimensions.
- **Cancellation kills the tree.** A cancelled invoke aborts immediately
  and terminates the child's whole process tree (process group on Unix,
  `taskkill /T /F` on Windows), never a background process still
  producing side effects. Covered by the existing cancellation tests.
- **Permissions cross the boundary.** The granted permission set is
  delivered to the child with each invocation (`granted_permissions`
  arrives intact — `granted_permissions_reach_the_child_intact`), so
  experimental code only ever acts on what was granted; the static
  sandbox is the enforcement layer for the environment and resource
  dimensions.
- **Scope boundary.** A per-invocation *brokered* FS/network proxy
  (filtering the child's file/network access by the granted set) is not
  part of this milestone — the sandbox is a static boundary (env, cwd,
  limits), the same shape the context service uses. Dynamic per-grant
  FS/network brokering is deferred until measurement shows the static
  boundary is the bottleneck (see "Later, only after evidence").

## V1-M12 Effect Runtime ✅ (implemented)

Every side effect — builtin tool or capability — routes through one
unified `EffectRequest` / `Effect` commit behind the runtime's generation
fence:

- **The contract is one channel.** `Effect` (commit/rollback, structured
  `EffectCommitError`), `ToolOutcome::PreparedEffect` for builtin tools and
  `CapabilityOutcome::EffectRequest` for capabilities; the capability-aware
  dispatcher forwards a capability's `EffectRequest` as a
  `PreparedEffect`, so the actor treats both identically. Capabilities
  stage via `WorkspaceHandle::prepare_write` (journaled mutation
  transaction) — the capability computes, the core executes.
- **The fence is one place.** `on_operation_completed` validates the
  operation against the generation fence: a stale completion (cancelled or
  superseded) rolls the staged effect back, a live one commits it. A
  cancelled tool or capability never mutates the workspace.
- **Commit failures are classified, not swallowed.** `NotApplied` tells
  the model nothing happened; `AppliedButDurabilityFailed` surfaces a
  degraded/recovery warning and tells the model the change *did* land but
  its record did not — the filesystem and the journal never silently
  disagree.
- **Coverage** (`agent-runtime/tests/turn.rs`): a live capability effect
  commits behind the fence and a stale one rolls back (cancelled operation
  → no avoidable stale mutation), plus the two commit-failure branches.
  The workspace mutation transaction itself (atomic replace, rollback
  deletes the staging file, durability-failure semantics) is covered by
  `agent-workspace`'s own tests.

## V1-M11 Context Recall ✅ (implemented)

Externalized information is pulled back on demand, and the prompt stays
refs-only while it is external:

- **Store injection is a composition-root choice.** `agent-tui` points the
  reference engine at `workspace.state_dir()/context-store`; a run started
  from a crate directory never scatters stores around the tree.
- **`ContextMapView` bounds the materialized external view at the type
  level** (cap 32, wire-validated; landed with Performance P1).
- **One retrieval surface.** `context.manage op=search|inspect|fetch`
  emits a typed `EngineQuery`; the actor routes it to the kernel, which
  resolves it against the engine (invariant 3 — the tool never touches
  the engine). `search` lists refs by entity/kind/scope/task, `inspect`
  is metadata without a store read, `fetch` reads the exact item content
  back from the store and stamps recency + GC generation on the entry —
  a deliberate read, not an automatic reactivation.
- **`gc_epoch`-based external TTLs** count real GC passes, and the store
  IO phases are async and batched (Performance P0/P2), so retrieval and
  eviction never stall the context hot path.
- **End-to-end acceptance test** (`agent-runtime/tests/recall.rs`): the
  full runtime loop through a real engine + real store — the model calls
  `context.manage op=fetch` → `EngineQuery` → actor → kernel → engine store
  read → the exact content returns in the tool result — while the
  materialized prompt carries only the bounded ref preview, never the full
  content. Acceptance: external information can be pulled back on demand
  without polluting the prompt.

## P2: provider/tool secondary issues ✅ (implemented)

- **Stream retry can no longer duplicate output.** `RetryingTransport`
  tracks whether anything reached the sink: a stream that failed *after*
  emitting deltas surfaces the error instead of retrying into the same
  sink (the live listener has no rewind); a failure before any delta is
  still retried. Covered by sink-recording tests.
- **Retry backoff is cancellation-aware.** Both `complete` and
  `complete_stream` select the backoff sleep against the request's
  cancellation token, so a cancelled request aborts immediately instead
  of sleeping out the wait.
- **Provider error bodies are bounded.** HTTP error bodies are truncated
  to a fixed cap in the error string, so a huge HTML error page cannot
  blow up the failure message.
- **OpenAI-compatible wire fields are per-provider configurable.**
  `OpenAiConfig::send_stream_options` and `send_max_tokens` control
  whether `stream_options`/`include_usage` and `max_tokens` are sent, so
  compatible providers that reject the fields work without code changes.

---

## Later, only after evidence

- vector recall;
- learned selection/ranking;
- counterfactual context evaluation;
- neural attention/selection;
- cross-session long-term memory;
- graph retrieval;
- multi-agent work.

These belong after the dynamic-lifecycle baseline is measurable.
