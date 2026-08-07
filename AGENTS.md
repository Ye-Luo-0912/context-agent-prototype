# Agent Development Contract

This repository is an experimental coding-agent runtime whose primary research target is **continuous context lifecycle management**.

Before changing architecture, preserve the invariants below.

## Non-negotiable invariants

1. **Do not make the conversation transcript the source of model context.**
   - Model input must be rebuilt from `ContextEngine::materialize`
     (`ContextQuery` -> structured `MaterializedContext`), then rendered by
     the runtime-owned `PromptAssembler`.
   - Never add a global append-only `messages: Vec<_>` to `AgentKernel` as the authoritative context mechanism.

2. **Token pressure must not be the trigger for forgetting.**
   - Context maintenance runs on runtime events (`UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`, `TaskCompleted`, `Checkpoint`).
   - Budgeting is only final packing.

3. **Tools must not access ContextEngine or memory stores.**
   - `tool-runtime` may depend on `agent-contracts` and workspace facilities.
   - Tool results return through `ToolOutput` to the kernel.
   - The kernel/context policy decides whether the observation enters context.

4. **Raw tool output is not prompt history.**
   - Large outputs go to `.focus-agent/artifacts/...`.
   - `ToolOutput::model_content` is explicitly bounded.

5. **Context implementation is replaceable.**
   - `agent-kernel` must depend only on the `ContextEngine` contract.
   - Do not import `context-simple` from the kernel.
   - Future ContextCore integration must be a new adapter/implementation, not a kernel rewrite.

6. **UI consumes runtime events.**
   - Do not bind TUI widgets directly to mutable kernel/context internals.
   - Grow `AppState` toward a reusable `RunStateAggregator`/view model.

7. **Raw traces are filesystem artifacts.**
   - Runtime traces/learning data use JSONL/artifact files in the prototype.
   - Do not add a database just to store raw traces.

8. **Keep v0 non-vector.**
   - No embedding/vector DB/RAG/graph retrieval until the dynamic working-set baseline is measured.

## Dependency rules

Allowed high-level direction:

```text
agent-contracts
  ^
  +-- context-simple
  +-- agent-workspace
  +-- tool-runtime (also -> agent-workspace)
  +-- agent-storage
  +-- agent-kernel

agent-tui -> composition of all implementations
```

Forbidden examples:

```text
agent-kernel -> context-simple       # forbidden
agent-kernel -> agent-tui            # forbidden
tool-runtime -> context-simple       # forbidden
tool-runtime -> ContextEngine        # forbidden
context-simple -> tool-runtime       # forbidden
agent-contracts -> concrete crate    # forbidden
```

## Context policy development

Before making the selection algorithm smarter, improve observability first.

Every context policy change should eventually be explainable as:

```text
item entered because ...
item selected because ...
item cooled/archived/dropped because ...
item reactivated because ...
model turn N consumed it
```

Prefer explicit features before opaque learned scoring:

- task/focus affinity;
- scope;
- retention;
- recency;
- access reinforcement;
- file/symbol/entity affinity;
- dependency/supersession relationships;
- verified error/fix status.

## Performance rules

- Keep database/network work out of the context hot path unless measurement proves it necessary.
- Use bounded channels and bounded model-facing tool output.
- Do not clone large tool results into events/context; store them once as artifacts.
- Do not optimize with `unsafe` before profiling shows a real hotspot.
- Streaming process output should use a bounded/ring buffer when implemented.
- Event persistence should remain buffered/off the runtime hot path.

## First implementation priorities

See `docs/ROADMAP.md`. In order:

1. compile/contract validation;
2. per-item context lifecycle instrumentation + replay;
3. one real streaming model provider;
4. practical coding tools (search/patch/git/process streaming);
5. A/B/C context-policy experiments;
6. smarter non-vector lifecycle policy;
7. ContextCore adapter.

## Definition of done for architectural changes

An architectural change is incomplete unless:

- dependency direction still satisfies this file;
- the new behavior has a bounded-output policy;
- runtime events expose enough state to debug it;
- a test covers the new context/tool lifecycle behavior;
- `docs/ARCHITECTURE.md` or `docs/CONTEXT_LIFECYCLE.md` is updated when the contract changes.
