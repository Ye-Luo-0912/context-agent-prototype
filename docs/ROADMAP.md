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
  `build_snapshot` expands the working set with dependencies of selected
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
  `maintain`, `build_snapshot`, `diagnostics`, `inspect`, `checkpoint`,
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

## Later, only after evidence

- vector recall;
- learned selection/ranking;
- counterfactual context evaluation;
- neural attention/selection;
- cross-session long-term memory;
- graph retrieval;
- multi-agent work.

These belong after the dynamic-lifecycle baseline is measurable.
