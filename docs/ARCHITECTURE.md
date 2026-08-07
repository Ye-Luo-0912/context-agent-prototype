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
      └──────── agent-kernel
                    ^
                    │
                 agent-tui
```

Important consequences:

- `agent-kernel` does not import `context-simple`.
- `tool-runtime` does not import `context-simple` or any memory implementation.
- `agent-tui` is the composition root and chooses concrete implementations.
- Future `context-contextcore` can implement `ContextEngine` without changing the kernel.

## 4. Stable contracts

### ContextEngine

The kernel needs four operations only:

1. `ingest` — submit a meaningful runtime observation.
2. `maintain` — trigger continuous lifecycle maintenance.
3. `build_snapshot` — construct the model-facing working context.
4. `diagnostics` — expose bounded observability.

The API is asynchronous even though `context-simple` is in-process. This leaves room for a future ContextCore service adapter over local IPC/HTTP/gRPC without changing the kernel contract.

Three implementations exist behind the same contract, plus the P5
process-boundary adapter:

- `context-simple::SimpleContextEngine` (C) — dynamic working set with
  lifecycle states (active/cooling/archived/dropped), focus affinity,
  scoring and transitions.
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
  the complete content for replay.
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

`ToolExecutionRequest` carries a `CancellationToken` (`cancel`), so long-running
work (searches, shell processes) can be aborted cooperatively by the caller.
Tool risk classes (`ReadOnly` / `WorkspaceWrite` / `ProcessExecution`) drive
approval before execution.

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
build_snapshot()
   │
   ├─ system constraints
   ├─ current FocusState
   ├─ selected Working Set
   └─ current user input
   │
   v
ModelTransport.complete()
   │
   ├─ final answer ───────────────┐
   │                              │
   └─ tool calls                  │
       │                          │
       ├─ approval                │
       ├─ ToolDispatcher.execute  │
       ├─ artifact raw output     │
       ├─ bounded ToolOutput      │
       ├─ ContextEngine.ingest    │
       └─ maintain(AfterTool)     │
             │                    │
             └──── next model ────┘

Final answer
   ├─ ingest(AssistantMessage)
   ├─ maintain(AfterModel)
   └─ TurnCompleted
```

## 6. Context is rebuilt, not replayed

The model-facing request is intentionally rebuilt from state instead of replaying an append-only transcript.

```text
System policy
+
Current FocusState
+
Selected Working Set
+
Current user input
```

This is the main architectural experiment.

A `ContextSnapshot` is a disposable projection. It is not the source of truth and should never be persisted as the memory model.

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
`agent-storage` and re-runs the recorded ingest/maintain/build_snapshot calls
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
ContextBuildRequest -> request op `build_snapshot`
ContextSnapshot     <- response payload
Diagnostics         <- response payload
```

The wire protocol carries only `agent-contracts` types — no ContextCore
vocabulary leaks through the Agent API. A real ContextCore runtime replaces
`agent-context-service` behind the same protocol; the kernel, tools,
approvals, TUI and provider are untouched (`agent-tui --context=service`).

Do not move Agent Kernel, tools, approvals, TUI, or provider code into ContextCore merely because ContextCore supplies context selection.

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
