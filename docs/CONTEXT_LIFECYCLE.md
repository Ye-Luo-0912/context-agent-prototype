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
5. materialize the working set as structured items (prompt assembly is
   the runtime's job, after the engine returns)
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

`/done <summary>` performs the intended lifecycle transition. Since V1-M2
the completion is a **task scope close**:

```text
task scope (and its focus child) -> Closed
durable outcomes (decision/finding/constraint/open-loop/
  artifact-ref/evidence-ref, pinned, durable) -> promoted to Session
the rest of the working set -> Archived
active focus -> cleared
summary -> Session + Durable
next user task -> new TaskId / new FocusState / new task scope
```

The close runs inside `maintain(TaskCompleted)` (not during ingest) so the
journal records an observable `task completed: scope closed, working set
evicted` transition for every evicted item. Promoted items move to the
session scope, become `Durable` and are tagged `promoted`; once their own
task is stale they sit at `Archived` (see the stale-task gate in §5) and can
be reactivated by a later task whose focus/entities match — recorded as a
`reactivated: score ... >= active threshold` transition.

This preserves the result while removing the completed task's detailed working set from active attention.

Automatic summary generation should be added at the runtime/task-boundary layer, not buried inside the context store.

## 8b. Scope tree (V1-M2)

The engine tracks a runtime scope tree as the first-class unit of
residency. It is separate from the item-level `scope` marker (§3): the
marker says which container an item semantically belongs to, the tree is
the container itself, with its own lifecycle.

```text
Session (one per run, opened lazily, never closed)
  └─ Task (one per task_id)
       └─ Focus (one per task while it runs)
            └─ Tool (one per tool call)
```

States: `Open` -> `Active` -> `Suspended` / `Closed`. The deepest active
scope is the current attention container (`active_scope_id`).

- `Session` opens lazily on the first ingest and lives for the run.
- `Task` opens with the focus; when the focus switches to another task
  the old task scope (and its focus) suspends instead of closing, so a
  later return re-activates them.
- `Focus` is the attention container of a task, active across turns;
  it opens with the first user message / focus change and closes when
  its task closes.
- `Tool` is an execution frame driven by the runtime (V1-P0-3): it opens
  when the tool starts (`ContextEngine::open_scope`) and closes when the
  next model round consumes the result (`ContextEngine::close_scope`).
  The observation persisted at turn end carries the tool scope id, so
  membership is authoritative even though persistence is batched. Tool
  scopes are containers, not eviction passes: durable members promote to
  the parent, ephemeral/working results leave through residency (§9) and
  error verification (§9b).

Scope close is uniform: durable outcomes are promoted to the nearest
open ancestor, the rest is released (a completed task's working set is
evicted; a focus close returns its working set to the task and the
normal lifecycle cools it). Since V1-P0-3 every item carries the
`scope_id` of the scope that produced it — membership is no longer
inferred from the `scope` marker, task id and creation time. Scope
counts are exposed through `ContextDiagnostics` and checkpoints
round-trip the tree.

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

Each item carries its **entity signature** (`ContextItem.entities`),
precomputed once at ingest after truncation. Dependency linking, supersession
/ verification queueing, entity-affinity scoring and GC root marking all
read the signature instead of re-parsing item content on every pass — the
ingest path is O(N) signature lookups, not O(N) re-extractions. Restore
backfills items from checkpoints written before the field existed.

At `materialize`, after primary selection, the working set is expanded
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

## 9d. V1-M6: full GC pass (residency / generation / semantic state)

The per-event `maintain()` pass is the **semantic** machine: it decides
Active/Cooling/Archived/Dropped and records every transition with a reason.
The full GC pass (`ContextEngine::gc`, run by the runtime actor at turn
boundaries) is the **physical** compactor. It keeps three dimensions
separate instead of folding them into one state enum:

```text
semantic state  ContextState::Active/Cooling/Archived/Dropped
                owned by the residency machine (maintain), not by GC
residency       ContextResidency::Resident | Evicted
                where the item physically lives (heap vs eviction buffer)
generation      gc_generation: u32
                full passes survived without being a root; roots reset to 0
```

### Mark phase (roots)

Roots are the current attention and are never swept while alive:

- pinned items (`Pinned` retention or scope);
- every item of the active task/focus scope (the working set is protected);
- durable session memory (task summaries promoted to the session scope);
- items whose entity signature overlaps the hot set (last user message +
  recent tool observations);
- a bounded transitive slice of their dependency edges (`+8`), so a working
  item keeps the items it depends on.

### Sweep phase

A **semantically Dropped item is evicted unconditionally** — the residency
machine already decided it is gone, so GC physically removes it from the
heap instead of letting it linger in checkpoints forever (a P2/perf issue).
An unmarked live item survives with `gc_generation += 1`; when that counter
clears `gc_max_generation` (default 3), or a non-durable item is older than
`turn_ttl_ticks * 4`, it becomes an eviction candidate. Active items are
never evicted: GC does not fight the policy — it compacts what the policy
demoted.

### Reversible eviction + reactivation

Eviction is **reversible**. Evicted items move to a bounded buffer
(`gc_buffer_capacity`, default 256) with `residency = Evicted` and an
`evicted_at_tick` stamp. Every `gc()` pass then scans the buffer (newest
first, `gc_reactivate_per_pass` max) and reactivates items that are relevant
again:

- pinned again;
- entities hot again in the working set;
- score still clears the active threshold.

Reactivation restores the item to the heap as `Active` and resets its
generation. Items evicted by the *current* pass are skipped, so nothing
bounces out and back in one GC. Only a buffer overflow is irreversible, and
it is bounded and counted (`purged`).

### Explainability

The `ContextGcReport` explains every decision:

```text
marked_roots / resident / evicted / reactivated / purged
evictions:      "survived 3 GC passes without root reachability (max 3)"
                "semantically dropped; evicted to reversible buffer (gen 1)"
                "stale: age 21 > ttl x4 = 20; not reachable from roots"
reactivations:  "entities are hot again in the working set"
                "score 0.61 >= active threshold 0.58"
diagnostics:    resident_items / evicted_items / gc_evicted_total /
                gc_reactivated_total
```

The runtime emits a `ContextGc` event after `AfterModel` maintenance; the
replay harness drives `engine.gc()` for every `ContextGc` event in a trace,
so the residency story (entered/selected/cooled/evicted/reactivated) is
replayable end to end. `SimpleContextConfig { gc_enabled, gc_max_generation,
gc_buffer_capacity, gc_reactivate_per_pass }` (all on by default) tune the
pass; `ContextEngine::gc` has a default no-op implementation, so baselines
and the wire adapter keep working unchanged.

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
  `total`). `materialize` also bumps `access_count` and
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
`SimpleContextEngine` with the same ingest / maintain / materialize calls
the runtime made (maintenance triggers are recorded on `ContextMaintained`
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

## 12. Turn Frame vs Context Frame (V1)

Since V1-M1 the model input is assembled in five layers, and tool
observations flow in two distinct phases:

- **During a turn** tool results live in the runtime-owned `TurnFrame` (the
  execution stack): they are rendered as protocol messages — an assistant
  message carrying `tool_calls`, then a `tool` message paired by
  `tool_call_id` — and they are never scored, garbage-collected or evicted.
- **When the turn ends** (the model stops calling tools) the observations
  are persisted as the long-term record: `ingest(ToolObservation)` for each
  result, then one `maintain(AfterTool)` pass, then the final assistant
  message and `maintain(AfterModel)`.

Consequences:

- no mid-turn duplication: the model sees each result once, in protocol
  form, and the context frame does not re-render the same observation;
- long-term memory still accumulates every observation (errors, verified
  fixes, decisions) — as a batch at turn end, so the error/supersession
  lifecycle observes the whole turn together;
- `AfterTool` maintenance now runs once per turn (at persist time) instead
  of once per tool call. The replay/A/B/C harnesses drive the engines
  directly and are unaffected by the kernel's timing.
