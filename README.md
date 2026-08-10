# Context Agent Prototype

A small Rust coding-agent runtime built to test one hypothesis first:

> Agent context should be a continuously maintained working set, not an append-only chat log that is compressed only when the context window fills.

This repository intentionally does **not** include vectors, RAG, knowledge graphs, multi-agent orchestration, or learned context ranking in v0.1.

## Architecture

```text
TUI / composition root
        │
        v
RuntimeInstance ── owns ── ModuleHost / capability registry
        │
        v
RuntimeActor (the only turn/task orchestrator)
        │
        v
AgentKernel (stateless trusted facade)
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
   turn, scope and prompt state. `agent-kernel` stays a stateless trusted
   facade and concrete implementations are wired only by the composition root.

## Workspace crates

- `agent-contracts`: stable cross-layer contracts.
- `context-simple`: first non-vector working-set implementation (dynamic).
- `context-baselines`: baseline A (append-only) and B (rolling-window + fixed
  marker, despite the legacy `RollingSummaryEngine` name) for A/B/C experiments.
- `context-contextcore`: `ContextEngine` adapter over a context-service process boundary (the ContextCore integration shape).
- `agent-context-service`: standalone context-service process speaking the adapter's JSON-lines protocol.
- `agent-workspace`: workspace root and artifact storage.
- `tool-runtime`: tool registry — file tools, `search.grep`, `edit.replace`, git status/diff, streaming `shell.exec`.
- `agent-storage`: append-only JSONL event journal.
- `agent-process`: framed child-process host, cancellation and sandbox hooks.
- `agent-capability-process`: process-capability adapter over `agent-process`.
- `agent-kernel`: stateless context/tool/model/approval/event facade.
- `agent-runtime`: sole actor/orchestrator, task state, prompt assembly,
  checkpoints, tool-surface planning and capability host.
- `agent-replay`: offline deterministic replay of a context lifecycle from a trace, plus the A/B/C scenario comparison.
- `agent-eval`: headless live-model smoke runner; it is not yet the real
  coding-workload acceptance suite.
- `provider-openai`: OpenAI-compatible streaming model provider (also DeepSeek/Qwen/Moonshot/GLM).
- `agent-tui`: minimal TUI and wiring; mock model by default.

## Run

The code is designed for modern stable Rust with the 2024 edition.

```bash
cargo run -p agent-tui -- .
```

The included `MockModelTransport` keeps the initial architecture runnable without binding the kernel to a model vendor. To use a real model, set `OPENAI_API_KEY` (and optionally `OPENAI_BASE_URL` / `OPENAI_MODEL`):

```bash
# DeepSeek example
$env:OPENAI_API_KEY = "sk-..."
$env:OPENAI_BASE_URL = "https://api.deepseek.com/v1"
$env:OPENAI_MODEL = "deepseek-chat"
cargo run -p agent-tui -- .
```

Any OpenAI-compatible endpoint works (OpenAI, DeepSeek, Qwen/DashScope, Moonshot/Kimi, GLM, ...). Output streams into the TUI; `/cancel` aborts the in-flight turn.

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

The builtin dispatcher provides eight repository tools plus two merged
runtime-control surfaces:

- `fs.list` / `fs.read` / `fs.write` — file browsing with bounded content;
- `search.grep` — regex search, ignores build artifacts/vendor dirs
  (`.git`, `target`, `node_modules`, ...), bounded hits;
- `edit.replace` — exact old→new patch (optionally `occurrence`/`replace_all`),
  records old content to the change journal;
- `git.status` / `git.diff` — workspace git state, bounded tail;
- `shell.exec` — streaming process execution: full log to an artifact,
  bounded ring-buffer tail to the model, `timeout_ms` and `/cancel` kill the
  child.
- `context.manage` — bounded GC hints/tags/leases/collect and external
  search/inspect/fetch/admit/derive requests routed by the runtime;
- `capability.manage` — paged catalog search/inspect/load/unload.

Every mutating tool also appends a `WorkspaceChange` record (tool, path,
action, old content when small) to `.focus-agent/changes.jsonl` — the
review/revert substrate for anything the agent changes.

## Useful commands in the TUI

- Type a message and press Enter.
- `/focus <text>` explicitly changes the current focus.
- `/pin <text>` inserts a pinned context item.
- `/done <summary>` archives the active task working set and retains the summary.
- `/context` prints current context diagnostics into the event stream.
- `/checkpoint` exports the context engine state to `.focus-agent/checkpoints/`.
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
`docs/CONTEXT_RUNTIME_TODO.md` (the code-grounded continuous-GC design queue),
`docs/TOOL_ECOSYSTEM_TODO.md` (modular trust boundaries, builtin ACI, and
extension ecosystem design queue),
and `docs/ROADMAP.md`.

## Current status

The dynamic-context and round-surface baselines are substantial, but this is
still a research prototype. Runtime transactions, external recall, exact
model-consumption acknowledgement and store-backed GC are implemented.
Cross-plane checkpoints, canonical context ownership, TaskAnchor/completion
semantics, process effect brokering, real filesystem/network isolation,
standing unattended-task policy and real coding non-inferiority remain open.
The code-grounded status table in `docs/ROADMAP.md` is authoritative; confirmed
defects and acceptance tests are tracked in `docs/AUDIT_TODO.md`.
