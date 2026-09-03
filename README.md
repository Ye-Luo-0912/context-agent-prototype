# Context Agent Prototype

A small Rust coding-agent runtime built to test one hypothesis first:

> Agent context should be a continuously maintained working set, not an append-only chat log that is compressed only when the context window fills.

This repository intentionally does **not** include vectors, RAG, knowledge graphs, multi-agent orchestration, or learned context ranking in v0.1.

The concrete product target is a reliable **single-user local coding Agent**:
one workspace, one explicit provider profile, bounded builtin tools,
human-approved effects, verifiable completion and cold resume. The runtime
substrate is substantial; installation, checked configuration, product-grade
resume/status and the formal M15 reliability gate are still open. The ordered
delivery plan is in [`docs/ROADMAP.md`](docs/ROADMAP.md#route-to-a-usable-local-agent).

## Architecture

```text
TUI (agent-tui) / composition root (agent-compose)
        │
        v
RuntimeInstance ── owns ── ModuleHost / capability registry
        │
        v
RuntimeActor (the only turn/task orchestrator)
        │
        v
agent-core CorePort (stateless trusted facade)
   ├─ ContextEngine ──> MaterializedContext ──> PromptAssembler
   ├─ ToolDispatcher ─> bounded ToolOutput / prepared Effect
   ├─ ModelTransport
   ├─ ApprovalGate
   └─ EventJournal ───> RuntimeEvent ──> TUI view state
```

## Hard boundaries

1. **Tools never access Context/Memory directly.** Tools return bounded model-facing output plus optional artifact references. The kernel decides what becomes context.
2. **The kernel depends only on contracts.** `context-simple` is a replaceable implementation; a future ContextCore adapter can implement the same `ContextEngine` trait.
3. **UI is event-driven.** It consumes runtime events and derives view state; raw runtime internals do not become the UI model.
4. **Forget continuously.** Context maintenance runs after meaningful runtime events, not only after token pressure.
5. **Budget is not forgetting.** The context budget is only the final packing constraint after lifecycle/attention decisions.
6. **Raw tool output is disposable.** Large output lives in artifacts; only a bounded observation enters the model working set.
7. **Message-scoped data expires by default.** Explicitly pinned items are the exception.
8. **There is one orchestrator.** `agent-runtime::RuntimeActor` owns task,
   turn, scope and prompt state. `agent-core` stays a turn-stateless trusted
   facade behind `CorePort` and concrete implementations are wired only by
   the composition root.

## Workspace crates

- `agent-contracts`: stable cross-layer contracts.
- `agent-platform-protocol`: bounded semantic wire DTOs for extension
  clients (parse-time JSON budgets, no transport/runtime).
- `context-simple`: first non-vector working-set implementation (dynamic).
- `context-baselines`: baseline A (append-only) and B (rolling-window + fixed
  marker, despite the legacy `RollingSummaryEngine` name) for A/B/C experiments.
- `context-contextcore`: `ContextEngine` adapter over a context-service process boundary (the ContextCore integration shape).
- `agent-context-service`: standalone context-service process speaking the adapter's JSON-lines protocol.
- `agent-workspace`: workspace root and artifact storage.
- `tool-runtime`: tool registry — file tools, `search.grep`, `edit.replace`, git status/diff, streaming `shell.exec`, `process.run`/`process.session`.
- `agent-process`: framed child-process host, cancellation and sandbox hooks.
- `agent-capability-process`: process-capability adapter over `agent-process`.
- `agent-storage`: append-only JSONL event and operation journals with fsynced durability barriers.
- `agent-core`: turn-stateless trusted Core facade behind `CorePort` (contracts, budgets, approval, events/audit/durability).
- `agent-conformance`: enforces the declared dependency layer/role graph.
- `agent-runtime`: sole actor/orchestrator, task state, prompt assembly,
  checkpoints, tool-surface planning and capability host.
- `agent-replay`: offline deterministic replay of a context lifecycle from a trace, plus the A/B/C scenario comparison.
- `agent-eval`: evaluation harness including the M15 formal-window runner.
- `provider-openai`: OpenAI-compatible streaming model provider (also DeepSeek/Qwen/Moonshot/GLM).
- `agent-compose`: trusted composition root that wires implementations and spawns the sole RuntimeActor.
- `agent-tui`: minimal TUI; one product host built on the composition root.

## Run

The code is designed for modern stable Rust with the 2024 edition.

```bash
cargo run -p agent-tui -- .
```

The included `MockModelTransport` keeps the architecture runnable without a
model vendor. **Configuration is checked at startup:** without
`OPENAI_API_KEY` the TUI refuses to start with a visible configuration
error, and `AGENT_DEMO=1` selects the mock transport explicitly. A no-key
run is a demo, never real Agent evidence. To use a real model, set
`OPENAI_API_KEY` and, when needed,
`OPENAI_BASE_URL`, `OPENAI_MODEL`, `OPENAI_API_PROTOCOL` and
`OPENAI_CONTEXT_WINDOW`:

```bash
# DeepSeek example
$env:OPENAI_API_KEY = "sk-..."
$env:OPENAI_BASE_URL = "https://api.deepseek.com/v1"
$env:OPENAI_MODEL = "deepseek-chat"
cargo run -p agent-tui -- .
```

The provider implements OpenAI-style Chat Completions and Responses streaming,
but endpoint capabilities differ. Treat a provider/model/protocol tuple as
supported only after its tool-calling and streaming preflight succeeds. Output
streams into the TUI; `/cancel` aborts the in-flight turn. The prototype TUI
currently inherits Core's 16-tool-round default, while formal M15 cells use a
separate 48-round cap; the product route requires an explicit checked cap and
tests against the value it actually ships.

For contributor/source readiness, the repository has one deterministic gate
runner:

```bash
cargo run -p agent-eval -- --doctor
```

It checks the source/toolchain/test environment and may optionally probe the
configured Provider. It is not yet an installed product `doctor`, and it never
starts a formal M15 preflight or window.

### Tool approval

Write and process tools ask before they act. By default the TUI shows an
`Approval Required` prompt (tool name + argument preview) for every
workspace-write or process-execution call:

- `y` / `Enter` allow the call;
- `n` / `Esc` deny it;
- read-only tools (`fs.list`, `fs.read`, `search.grep`, `git.status`,
  `git.diff`) always run without prompting.

If no one answers, the request is auto-denied after a timeout (default 5
minutes) so a turn can never hang. For fully automatic policy, start with
`--read-only` — every write/process call is denied by policy:

```bash
cargo run -p agent-tui -- --read-only .
```

### Builtin tool set

The production-default model surface contains `fs.list`, `fs.read`,
`fs.write`, `artifact.read`, `search.grep`, `edit.patch`, `git.status`,
`git.diff`, `task.complete` and `capability.manage`. When the host discovers a
bounded verification recipe, `verify.run` is also required.

`fs.mkdir`, `edit.replace`, `shell.exec`, `process.run`, `process.session`,
`code.symbols`, `code.diagnostics`, `context.manage` and `task.manage` are
catalog-optional and load through `capability.manage`; they do not all occupy
every model round. Tool visibility never grants effect authority. Detailed
schemas, limits, effects and disposition are recorded in the reviewed
[`docs/TOOL_INVENTORY.json`](docs/TOOL_INVENTORY.json); the Rust registry
remains authoritative until the generated-manifest residual closes.

Every mutating tool also appends a `WorkspaceChange` record (tool, path,
action, old content when small) to `.focus-agent/changes.jsonl` — the
review/revert substrate for anything the agent changes.

## Useful commands in the TUI

- Type a message and press Enter.
- `/focus <text>` explicitly changes the current focus.
- `/tasks` lists known tasks; `/task <id>` activates one.
- `/suspend` suspends the active task.
- `/pin <text>` inserts a pinned context item.
- `/done <summary>` archives the active task working set and retains the summary.
- `/context` prints current context diagnostics into the event stream.
- `/checkpoint` exports a cross-plane runtime checkpoint to
  `.focus-agent/checkpoints/`.
- `/restore <path>` restores a runtime checkpoint in the current prototype.
  This path-based command is not yet the verified `resume latest` product flow.
- `/grants` lists active standing grants. Startup grants currently use
  `--grant=<JSON>`; a usable revoke flow is still product work.
- `/cancel` aborts the in-flight model turn.
- `Tab` toggles the context inspect panel (selected items + lifecycle transitions).
- `/quit` exits.

## Replay a run trace

```bash
cargo run -p agent-replay -- .focus-agent/traces/<run>.jsonl
```

Prints, per context item: what entered and why, which successful model turns
committed its exact id through `ContextConsumed`, every state transition with
its reason, and the final state. Legacy traces without consumption events keep
their original replay semantics.

## A/B/C context-policy experiments (P3)

The same runtime can run against three context policies — append-only (A),
rolling-window marker (B), dynamic working set (C, default) — plus the
process-boundary adapter (P5):

```bash
cargo run -p agent-tui -- --context=append .
cargo run -p agent-tui -- --context=rolling .
cargo run -p agent-tui -- --context=dynamic .
cargo run -p agent-tui -- --context=service .   # spawns agent-context-service
```

Compare all seven scripted scenarios offline (deterministic, no model
needed):

```bash
cargo run -p agent-replay -- --compare          # all scenarios
cargo run -p agent-replay -- --compare long_refactor
```

The comparison table reports total/max input tokens, over-budget snapshots,
lifecycle churn and final working-set size per engine. Full results and
reading notes: `docs/EXPERIMENTS.md`.

The dynamic engine (C) adds four explicit, explainable rules (P4): later
decisions supersede earlier ones on the same entities, failed tool results
persist until a later success verifies the fix, entity affinity rewards
items whose files/symbols are hot, and an explicit dependency graph pulls
dependencies of selected items into the working set — all recorded as
lifecycle transitions / dependency edges and covered in
`docs/CONTEXT_LIFECYCLE.md` §9b–9c.

See `docs/ARCHITECTURE.md`, `docs/CONTEXT_LIFECYCLE.md`,
`docs/CONTEXT_RUNTIME_TODO.md` (historical design queue; not a live
authority), `docs/TOOL_ECOSYSTEM_TODO.md` (modular trust boundaries, builtin
ACI, and extension ecosystem design queue), and `docs/ROADMAP.md`.

## Current status

The dynamic-context and round-surface baselines are substantial, but this is
still a source-built research prototype. Runtime transactions, external
recall, model-consumption acknowledgement, store-backed GC, cross-plane
checkpoints, TaskAnchor/completion semantics, process effect brokering, typed
settlements and bounded process authority are implemented. The main gaps are
now the product entry/configuration, verified workspace resume, visible
recovery/status UX, a few correctness residuals and real coding
non-inferiority.

Formal M15 remains open: seven v4 valid FAIL windows are retained and the
latest is 10/12. Successor reliability commit `b44ea44` passed the complete
local doctor. Follow-up `c84f85e` passed both platform check jobs but exposed a
parallel fd-reuse race in one Ubuntu assertion; the current identity-aware test
fix still needs a clean CI record. See the current snapshot before interpreting
any historical chronology.
`docs/STATUS.md` owns Now/freeze status, `docs/ROADMAP.md` owns milestone
gates/order, `docs/AUDIT_TODO.md` owns confirmed defects and acceptance
tests, and `docs/M15_ACCEPTANCE.md` owns the formal-window design.
