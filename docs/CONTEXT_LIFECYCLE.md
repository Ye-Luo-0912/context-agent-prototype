# Context Lifecycle v0.1

## 1. Core rule

**Forgetting is a normal runtime operation, not an emergency response to token pressure.**

The lifecycle runs after meaningful events even when there is abundant model context capacity.

## 2. State model

```text
                   relevant / accessed
              ┌─────────────────────────┐
              │                         │
              v                         │
          ┌────────┐                ┌─────────┐
new ----> │ Active │  decay ------> │ Cooling │
          └───┬────┘                └────┬────┘
              │                          │
              │ low relevance            │ low relevance
              v                          v
          ┌──────────┐  renewed      ┌──────────┐
          │ Archived │ ------------> │ Active   │
          └────┬─────┘               └──────────┘
               │
               │ ephemeral/obsolete/TTL
               v
          ┌─────────┐
          │ Dropped │
          └─────────┘
```

`Dropped` means removed from the online working set. Raw artifacts and the runtime event journal are separate concerns.

## 3. Retention and scope are separate

### Scope

- `Message`: valid only for one message unless promoted/pinned.
- `Turn`: normally valid through the current tool/model turn.
- `Task`: belongs to the active task.
- `Session`: can survive task transitions.
- `Pinned`: explicitly forced into the working set.

### Retention

- `Ephemeral`: expected to disappear quickly.
- `Working`: normal active-task material.
- `Durable`: can be archived and later reactivated.
- `Pinned`: cannot be evicted by normal decay.

This separation prevents a single overloaded “importance” number from deciding every lifecycle behavior.

## 4. Maintenance triggers

The runtime invokes maintenance on:

```text
UserInput
BeforeModel
AfterModel
AfterTool
FocusChanged
TaskCompleted
Checkpoint
```

This is intentional. A future smarter engine can use different policies per trigger without changing the Agent Kernel.

## 5. v0.1 scoring

The first engine uses cheap explicit signals only:

```text
score ≈
  intrinsic importance
+ current goal/query lexical overlap
+ active entity match
+ entity affinity (hot working-set entities, P4)
+ task/scope affinity
+ recency
+ access reinforcement
+ retention bonus
```

`entity_affinity` (P4) is 0.18 × the fraction of an item's entity signature
that is currently hot — the hot set is seeded by the last user message and
extended by entities touched in tool observations (cap 24, reset on user
message / focus change). It overlaps `active entity match` for user-message
entities and additionally covers files/symbols the agent actually touched via
tools; both components are reported separately in the score breakdown.

One explicit gate sits on top of the raw score: **items belonging to a task
that is not the active focus** (including a completed task after focus is
cleared) are capped at `Archived` unless the current focus pushes their score
at or above the active threshold. This keeps completed-task details from
leaking back into active attention on recency alone (task-switch
contamination); a strong focus match can still reactivate them, and that
reactivation is recorded as a `reactivated: score ... >= active threshold`
transition.

No embedding or semantic vector score exists in this implementation.

The formula is not intended to be “the answer”. It is instrumentation-friendly scaffolding for experiments.

## 6. Budget behavior

Correct order:

```text
1. lifecycle maintenance
2. score/select candidate working set
3. reactivate/archive as appropriate
4. pack selected candidates into model budget
5. build ContextSnapshot
```

Incorrect order:

```text
append everything
-> hit token limit
-> summarize old prefix
-> append again
```

The budget is a hard physical constraint, not the memory policy.

## 7. Focus transitions

A FocusState contains:

- task id;
- task goal;
- current query;
- phase;
- active entities;
- generation.

The prototype supports explicit `/focus <goal>` to force a new task focus.

The first normal user message creates a focus. Later user messages update the current query while keeping the task goal. A future task-boundary detector can replace this explicit/simple behavior.

## 8. Completing a task

`/done <summary>` performs the intended lifecycle transition:

```text
active task items -> Archived   (recorded as a transition during maintain(TaskCompleted))
active focus      -> cleared
summary           -> Session + Durable
next user task    -> new TaskId / new FocusState
```

The archiving itself runs inside `maintain(TaskCompleted)` (not during ingest)
so the journal records an observable `task completed: working set archived`
transition for every affected item. Once archived, those items stay out of
active attention for subsequent tasks (see the stale-task gate in §5) unless
the new focus strongly reactivates them.

This preserves the result while removing the completed task's detailed working set from active attention.

Automatic summary generation should be added at the runtime/task-boundary layer, not buried inside the context store.

## 9. Tool observations

Successful observations are initially:

```text
scope     = Turn
retention = Ephemeral
```

They remain available for the immediate model continuation, then are eligible for hard removal after `AfterModel`.

Failed observations follow the P4 error lifecycle instead (§9b): they persist
as `Working` until a later successful result on the same entities verifies
the fix, then are archived with an explainable transition.

## 9b. P4: supersession and the error lifecycle

Three experiment-driven rules make selection smarter without learned scoring.
All state changes are ordinary lifecycle transitions with explicit reasons.

### Supersession

A user message that reads as a decision (keyword-based: "use X", "switch to
Y", "revert", "drop Z", ...) is tagged `decision` and promoted (importance
0.72). When a later decision shares an entity (token with a path/name/case
signature) with an earlier decision item, the earlier one is superseded:
archived with reason `superseded by decision at turn N: '<content>'` and
permanently excluded from model requests — whatever its score.

```text
user: "use TOML for config"        -> item A (decision)
user: "switch to YAML instead"     -> item A archived (superseded), item B active
```

This fixes the P3-measured regression on the `superseded_decisions` scenario
(superseded decisions used to be re-ingested as fresh items).

### Error -> fix -> verified

A failed tool observation is an `Error` item with `Working` retention: it
survives the model turn so a later attempt can confirm the fix.

- a new failure sharing entities with a live error supersedes it
  (`recurring failure supersedes earlier error (round N, same entities)`) —
  one live error per failure site, always the latest;
- a successful result sharing entities verifies the fix
  (`error verified fixed by successful tool result (round N)`) and archives
  the error;
- superseded/verified errors never re-enter a model request.

```text
round 1: tests failed (AuthService.rs:42)  -> Error item E1 (live)
round 2: tests failed (AuthService.rs:41)  -> E1 archived (recurring), E2 live
round 3: tests passed (AuthService.rs)     -> E2 archived (verified fixed)
```

Trade-off, measured: keeping errors visible until verified costs a little
more input than dropping them after one turn (they stay in the working set),
but the peak stays far below the budget and the failure diagnosis stays
available across retries.

### Decision and constraint promotion

- decisions: tagged + importance 0.72 (see above);
- constraints (pins): unchanged — `Pinned` retention, always selected.

These rules are configurable and can be switched off for comparison:
`SimpleContextConfig { supersession, error_verification }` (default on);
`SimpleContextConfig::baseline_v0()` reproduces the P3-era policy.

## 9c. P4: entity affinity and the explicit dependency graph

Two more explicit, non-learned signals make the working set follow what the
agent is actually touching.

### Entity affinity

The engine keeps a bounded **hot-entity set** (cap 24): seeded by the last
user message (`extract_entities(content)`), extended by entities appearing
in tool observations (most recent first), and reset by a new user message or
`FocusChanged`. An "entity" is a cheap signature — a whitespace token of
length ≥ 3 carrying a path/name/case marker (`.`, `/`, `::`, `_` or an
uppercase letter).

Scoring gains `entity_affinity = 0.18 × (fraction of the item's entities in
the hot set)`, zero when the item or the hot set has no entities. It is
reported in `ScoreBreakdown.entity_affinity` and the selection reason
(`affinity=...`), so a replay can explain "item selected because its file
was just touched by a tool result".

```text
user: "fix AuthService.rs"            -> hot = {AuthService.rs}
tool: "tests passed in CacheStore.rs" -> hot = {CacheStore.rs, AuthService.rs}
item "patch AuthService.rs"           -> affinity 0.18 (1/1 entity hot)
item "work on TokenCache"             -> affinity 0 (no hot entity)
```

### Explicit dependency graph

At ingest, every new item records up to 8 **dependencies**: prior non-dropped
items sharing at least one entity (new item depends on prior). Edges are
exposed via `ContextItemSummary.dependencies` and rendered by the replay
report (`depends on: <id>`).

At `build_snapshot`, after primary selection, the working set is expanded
with dependencies of selected items:

- skipped: already-selected items, `Dropped`, and `superseded` /
  `verified-fixed` items (a verified error or superseded decision never
  re-enters through the back door);
- `Archived` dependencies only when their score still clears the active
  threshold (same gate as primary selection — expansion never resurrects
  cold archived items);
- best dependencies first (score desc), capped at +8 per snapshot;
- spends only a **1 K-token reserve** carved out of the model budget, so the
  snapshot can never exceed the budget (an over-budget guarantee covered by
  tests);
- reason: `included as dependency of item <short-id>`.

```text
item E (Error, "AuthService.rs:42")          -- verified, archived, excluded
item F (ToolObservation, "passed AuthService.rs") -- depends on E (same entity)
snapshot selects F -> expansion may re-add E only if E still clears the
active threshold; because E is verified-fixed it never re-enters.
```

Measured effect (see `docs/EXPERIMENTS.md` §6): six of seven scenarios are
unchanged; `long_refactor` pays +0.9 K tokens for same-file traceability and
`superseded_decisions` churn drops 60 → 40 (affinity keeps the hot decision
stable).

Both features are configurable: `SimpleContextConfig { entity_affinity,
dependency_expansion }` (default on);
`SimpleContextConfig::baseline_v0()` turns all four P4 flags off.

## 10. What should become durable later

A later policy can promote only structured outcomes such as:

- final decisions;
- persistent user/project constraints;
- task summaries;
- verified file/project facts;
- reusable failure diagnoses;
- unresolved blockers.

Transient shell output, repeated explanations, stale plans, and superseded attempts should normally remain ephemeral or archived.

## 11. Observability (P0.5, implemented)

Replay is the acceptance test: given a run's JSONL trace, `agent-replay` can
answer exactly:

- what entered context (item kind, scope, `source`, entry turn);
- why it entered (source/kind + turn attribution);
- when it left (turn-stamped `ContextStateTransition`);
- why it left (transition reason string);
- which model turns consumed it (selections recorded per `ContextPrepared`);
- whether it was later reactivated (`Archived/Cooling -> Active` transitions).

### 11.1 What the journal records per item

- **Entry**: item `created_turn` plus `source` (user / assistant / tool:<name> /
  explicit-pin / task-summary) and kind.
- **Selection**: every `ContextPrepared` event carries the selected items with
  `score`, `approx_tokens`, and a `ScoreBreakdown`
  (`importance`, `focus`, `recency`, `access`, `scope_bonus`, `retention_bonus`,
  `total`). `build_snapshot` also bumps `access_count` and
  `last_access_turn` on consumed items.
- **Transitions**: every `ContextMaintained` event carries
  `report.transitions: Vec<ContextStateTransition>` with `item_id`, `kind`,
  `scope`, `from`, `to`, `turn`, and a human-readable `reason`
  (e.g. `ephemeral ToolObservation observation dropped after model turn 1`,
  `decayed: score 0.31 below active threshold 0.58`,
  `archived: score 0.12 below archive threshold 0.24`,
  `reactivated: score 0.71 >= active threshold 0.58`).
- **Turn / tool-round ids**: the engine assigns `turn` (incremented per user
  message) and `tool_round` (incremented per tool observation) internally; both
  are exposed via `ContextDiagnostics` and stamped onto items and transitions.

### 11.2 Deterministic replay

`crates/agent-replay` walks a JSONL trace and drives a fresh
`SimpleContextEngine` with the same ingest / maintain / build_snapshot calls
the kernel made (maintenance triggers are recorded on `ContextMaintained`
events). Output is a per-item lifecycle report plus final diagnostics.

```bash
cargo run -p agent-replay -- .focus-agent/traces/<run>.jsonl
```

Replay is deterministic for a given engine version: the recorded transitions
are ground truth, and replay re-derives the same story from the events.

### 11.3 Checkpoints vs event history

`/checkpoint` (or `ContextEngine::checkpoint`) exports the engine's runtime
state — items, focus, tick/turn counters — to a separate JSON file under
`.focus-agent/checkpoints/`, independent of the event journal. `restore`
replaces engine state from such a file. This keeps durable runtime state
separate from the append-only trace used for learning/replay.
