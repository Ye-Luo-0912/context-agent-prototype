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

Dependency expansion respects it too: the expansion slice is spent only
from the reserved budget, with no pinned exemption — a pinned dependency
cannot break the frame. And the engine's budget is a *target*: the
runtime re-estimates the fully assembled request (system + focus +
context + turn + tool schemas, including the rendering overhead the
engine never sees) and trims the context frame if the wire estimate
exceeds the provider window. The engine proposes; the runtime disposes.

## 6b. The selection universe: secondary indexes

Since V1-M9 the engine keeps slot-based secondary indexes beside the
heap (`IdIndex` item-id → slot, `EntityIndex` entity → item ids,
`ScopeIndex` scope id → item ids, plus the unscoped bucket for legacy
checkpoints). They answer the two questions the hot path used to scan
the whole heap for:

- **dependency ingest** previously matched a fresh item against every
  prior item (O(heap) with string parsing per pair); it now reads the
  entity buckets directly — O(entities × bucket), newest-first via slot
  order, capped per entity and per item. Precomputed entity signatures
  mean no content is re-parsed during ingest;
- **materializer candidate generation** no longer walks the full heap.
  The selection universe is explicit: items of the active task's scope
  subtree (session scope + the current task's scopes, including closed
  tool frames of that task), items whose entities are hot, and legacy
  unscoped items. Everything else is not scoreable this snapshot.

The index is a cache over the heap, kept consistent at every structural
mutation (insert on push, wholesale rebuild after a GC sweep, scope
re-stamp on promotion). `ensure_consistent` re-derives it when a caller
mutated the heap without the helpers, so a stale index can never
silently drop candidates.

## 7. Focus transitions

Since V1-M9 the runtime separates **tasks** from **focus**: a `TaskManager`
(`agent-runtime::task`) owns the long-lived execution entities — `TaskRecord
{ id, goal, status, timestamps }` with `create_task` / `activate_task` /
`suspend_task` / `complete_task`, and dedupe by goal so re-focusing a
previous goal resumes the same task id. Focus is only the current attention
*inside* a task; `set_focus(task_id, focus)` names an existing task and
never mints a new one.

```text
/focus A  → create_task("A")  → Task 1 active, focus on Task 1
/focus B  → create_task("B")  → Task 2 active, Task 1 suspended
/focus A  → activate_task(Task 1) → Task 1 active again, Task 2 suspended
```

This is what scope suspension/resume and the GC rely on: a resumed task
re-activates its scope instead of opening a fresh one, and completed tasks
stay completed instead of being re-created by a later focus.

A FocusState contains:

- task id;
- task goal;
- current query;
- phase;
- active entities;
- generation.

The prototype supports explicit `/focus <goal>` to force a new task focus.

Task identity is runtime-owned: only the `TaskManager` creates tasks, and
focus is always established through `set_focus(task_id, goal)` → a
`FocusChanged` ingest. The engine never mints a `TaskId` — when the first
normal user message arrives with no active task, the runtime auto-creates
an *implicit* task, sets focus to it, and only then ingests the message, so
the message lands in a real task scope. A `UserMessage` that still arrives
with no focus (engine used directly) is a session-level message: it falls
back to the session scope and stays selectable, but no focus is invented.
Later user messages update the current query while keeping the task goal. A
future task-boundary detector can replace this explicit/simple behavior.

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

Promotion is decided before the attention state is consulted: an item the
residency machine already cooled to `Archived` still gets its promotion
chance when it is a durable outcome of the closing scope (only
`Dropped` and semantically excluded items are skipped). The promotion also
rewrites the authoritative `scope_id` membership to the parent scope, so a
later close of the parent still sees the item.

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

> Historical snapshot of the M6 design. V1-M9 (§9e) supersedes three parts:
> the `ContextState` enum (split into attention/semantic axes), the root set
> (task membership removed) and the buffer overflow behavior (store-backed
> instead of purge). The rest of this section still describes the pass.

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
  item keeps the items it *depends on*: the traversal follows
  `item.dependencies` (new → old) outward from the roots. Dependents of a
  root are not protected — a root's descendants carry no evidence the
  working set relies on.

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
bounces out and back in one GC. Semantically dead items never resurrect:
a superseded decision or a verified-fixed error (both label-excluded from
the model) stays in the buffer however hot its entities look — reviving
them to `Active + Resident` while `is_excluded` blocks them forever would
be a state-space inconsistency. Only a buffer overflow is irreversible, and
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

## 9e. V1-M9: attention / semantic split, store-backed eviction, tightened roots

The M6 pass kept one `ContextState` enum; the V1-M9 rework splits the
dimensions the way M6 intended, fixes the mark direction, and removes the
last "permanent delete" from the context GC.

### State model: three orthogonal axes

`ContextState` is gone. `ContextItem` now carries:

```text
attention  AttentionState::Active | Cooling | Archived
           owned by the residency machine (maintain); GC never moves it
semantic   SemanticState::Live | Superseded{by} | VerifiedFixed{by} |
           Tombstoned
           owned by the policy; once terminal (Superseded / VerifiedFixed /
           Tombstoned) it is permanent — nothing ever sets it back to Live
residency  Residency::Resident | Warm | Cold | External
           where the item physically lives (heap / buffer / store)
generation gc_generation: u32   full passes survived without being a root
```

`Dropped` no longer exists. Ephemeral observations are `Archived` after
their turn (consumed) but stay semantically `Live`; semantic death is
expressed only by `SemanticState`, and **a tombstone never resurrects**:
reactivation skips any item whose semantic state is terminal, however hot
its entities look — `is_excluded()` and the state machine can no longer
disagree (`Resident + Active` but never visible is impossible now).

### Mark phase: dependency direction fixed

Roots are marked, then the traversal follows **`item.dependencies`
(new → old)** outward: a root's *dependencies* are the evidence it relies
on and are protected; its dependents are not. (The M6 text described this
direction but the implementation walked `dependencies` backwards — dependents
got marked while the roots' actual evidence was swept. The implementation
now matches: `queue = roots; pop id; mark item[id].dependencies; push them`.)
The root set is also tightened — **the active task is a scope boundary, not
a root**: an item does not survive because its `task_id` matches the active
task. Roots are only pinned items, the current focus scope, open loops,
durable task constraints, hot entities and explicit references (plus a
bounded `+8` transitive slice of their dependency edges). Old turns of a
long task therefore cool and evict like any other working-set item.

### Eviction is reversible all the way down: ContextStore

The bounded eviction buffer no longer purges on overflow. The residency
ladder is:

```text
Resident -> Warm -> Cold -> External   (context GC)
External -> Storage GC -> Delete       (a future, separate pass)
```

`ContextEngine` gains a `store` surface (`ContextStore`): when the buffer is
full, the oldest eviction is written to the store as an `ExternalizedContext`
(a `ContextRef` like `context://run/task/item-id`, plus `artifact://` /
`decision://` / `evidence://` links when the item carries them) and its
residency becomes `External`. The model never sees the body — only the light
`ContextRef` in the materialized view — and the full item can be restored
with `ContextEngine::restore(ref)` (a `materialize`-visible `Restored` item
whose body lives in an artifact, or back into the heap when it is hot again).

### Scope close: promote before archive, and keep membership in sync

Closing a scope used to skip `Archived` items outright, so a durable outcome
(decision / finding / constraint / open loop) that had cooled could miss
promotion to the parent scope. The order is now: terminal-semantic-invalid
items are skipped; then `should_promote` wins and the item is promoted
(updating **both** `scope` and `scope_id` to the parent — previously only
`scope` was rewritten while the authoritative `scope_id` kept pointing at
the closed scope); everything else is archived/evicted as before.

### 9f. V1-M9: adaptive runtime — the model can steer the working set

The GC is reversible and explainable, but until now only the policy decided
what survives. Since V1-M9 the model can *ask* the runtime to keep an item,
attach a searchable tag, lease an item for a bounded number of turns, or run
a full GC pass — without ever touching the engine (invariant 3: tools return
`ToolOutput`; the kernel routes the directive).

Four read-only meta-tools (`context.gc_hint` / `context.tag` /
`context.lease` / `context.collect`, always loaded with the core tool set)
return a `ToolOutcome::RuntimeDirective` carrying a typed `ContextAction`
(invariant 3 still holds — tools return `ToolOutput`-shaped results and
never touch the engine; the kernel routes the directive). The actor
executes the directive at **operation-commit time**, inside the same
generation fence that guards effect commit, so a hint lands before the
very observation it targets (and before the next model round sees it):

- `Collect` — the actor calls `ContextEngine::gc()` immediately and emits
  `RuntimeEvent::ContextGc { report }`. The runtime owns the GC pass; a
  collect directive never enters the engine as ingest.
- everything else — `ContextIngress::ContextDirective { action }`, applied
  by `apply_directive`: `gc_hint` sets/clears `keep_alive`, `tag` pushes a
  deduped `Label::extension(tag)`, `lease` stamps `lease_until_turn =
  turn + min(turns, max_lease_turns)`.

Producing a `RuntimeDirective` requires the `runtime:context-control`
permission in the capability manifest; a tool without it gets its directive
rewritten into a denied `Value`. Hints and leases are bounded, not
permanent roots: `keep_alive` is capped (`max_keep_alive_items`), leases
are capped per task in count (`max_leased_items_per_task`) and weight
(`max_leased_tokens_per_task`), and a quota refusal surfaces as an
`InvalidRequest` error from the directive ingest so the model learns its
request was not granted. Both protections auto-expire when the owning task
completes.

The engine searches the heap *and* the eviction buffer for the target id,
so a hint/lease on an already-evicted item reactivates it on the next GC
pass (`kept alive by a model gc_hint` / `leased by the model until turn N`);
a stale id (item already externalized or semantically dead) is a silent
no-op — the tools are safe to call even when the target just left.

GC treats model direction as a root claim: `keep_alive` or a live lease
marks an item `model_directed_root` in the mark phase, and in the sweep an
explicit directive overrides the consumed-ephemeral heuristic — a spent turn
observation the model asked to keep stays resident. Everything stays
explainable in the ledger (`kept because the model leased it until turn N /
set keep_alive`), and the effect is visible to the model because the context
frame exposes each item's id (`id=<...>`), so the next request can target it.

### 9g. V1-M9: the external store becomes a retrieval surface

The store path is no longer guessed from the CWD. The composition root
injects `workspace.state_dir()/context-store` into
`SimpleContextConfig::context_store_dir` (`agent-tui`), so a run started
from a crate directory can no longer scatter `.focus-agent/context-store`
folders around the tree. The leaked tracked copy was deleted and
`.gitignore` covers `**/.focus-agent/` (not just the repository root).

Externalized is not deleted, and since V1-M9 it is also *findable again*:
`ContextEngine` gains a deterministic retrieval surface —
`search_external(ContextSearchQuery { query, kind, scope, task_id, limit })`,
`inspect_external(item_id)` and `fetch_external(item_id)` (default no-ops,
so baselines and the wire adapter are untouched). Three always-loaded
read-only meta-tools (`context.search` / `context.inspect` /
`context.fetch`) produce an `EngineQuery` the kernel resolves against the
engine (invariant 3 — tools still never touch the engine). The prompt's
`EXTERNAL CONTEXT` section shows refs only (uri + kind/scope + summary) and
explicitly points the model at the retrieval loop; `fetch` stamps recency
and the GC generation on the entry so ranking and Cold -> External aging
stay honest. A fetch is a deliberate read, not an automatic reactivation —
the model decides what re-enters the working set.

The materialized `external` field is a bounded `ContextMapView`, not a
clone of the whole map: at most 32 refs, quickselect-ranked in O(n)
without cloning it (hot-entity match first, then open-loop tags, then
recency, with a deterministic id tie-break). A run with 10 K / 100 K
external refs costs the same per materialize as a run with 32.

Cold recall pre-filters in memory instead of reading the disk:
`ExternalizedContext` keeps the item's entity signature (`entities`),
`tags`, `dependencies` and `task_id`, so with thousands of Cold entries
only the entity-matching ids are read in the IO phase (10 K refs -> a few
reads), never the reverse.

External TTLs count *generations*, not ticks. `State::gc_epoch`
increments only on a full GC pass; `ExternalizedContext::last_access_gc_epoch`
records the pass it was last touched; `gc_external_ttl_generations`
replaces the pass/tick-confused `gc_external_ttl_passes`. "4 generations"
means 4 real GC passes, not 4 unrelated runtime operations (ingest /
maintain / materialize also grow `tick`). Entries restored from pre-epoch
checkpoints (`last_access_gc_epoch == None`) start fresh at the current
epoch instead of aging out instantly.

GC no longer holds the async state lock across synchronous disk IO. One
full pass is three phases: `plan_full_gc` (under the lock — mark / sweep /
reactivate, decide the externalize list and the recall candidates, bump
`gc_epoch`), then `run_store_io` (lock released — `externalize_async` /
`read_item_async`), then `commit_full_gc` (fresh lock — entries join the
map, recalled items re-enter the heap, failed writes return to the front
of the buffer so overflow retries next pass). The tail latency of a
growing store stops being synchronous with the state lock.

Storage GC is the only place information is permanently deleted, and it is
now a *reachability closure* rather than a single incoming-edge check:
roots are the dependency edges of resident/warm items, and every reachable
external entry contributes its own `dependencies`, so external -> external
chains, semantic links and future audit/evidence/OpenLoop edges keep their
targets alive transitively. `delete_file` returns
`Result<DeleteOutcome>` (`Deleted` / `NotFound`): a real IO error
(permission, disk) keeps the in-memory entry and surfaces in
`StorageGcReport.io_errors`, instead of being mistaken for "the file is
gone" and dropping the metadata while the content still exists.

### 9h. V1-M9: index consistency is structural (ContextHeap)

`State.items` was a bare `Vec<ContextItem>` any module could mutate while
the slot/entity/scope indexes silently drifted — the consistency guard
could only catch length changes, never a same-length `entities` or
`scope_id` edit. The heap now owns its indexes: `ContextHeap` exposes only
structured mutations — `push` (index at its slot in one step),
`replace_all` (GC sweep / restore), `take_all`, `update_scope(index, from,
to)` (field write + bucket move atomic), `update_entities(index, entities)`
(old buckets removed, new added) — plus read-only iteration, so a stale
index is a type error instead of a runtime heuristic. Non-indexed fields
(semantic state, tags, keep_alive, lease, access stamps) stay reachable
through `iter_mut`, which cannot affect any index bucket.
`ensure_consistent` survives only as a len-guard safety net for test-only
direct pushes; the production mutation surface no longer needs it.
Checkpoints serialize only the items; the indexes are derived state and
rebuild on restore.

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
