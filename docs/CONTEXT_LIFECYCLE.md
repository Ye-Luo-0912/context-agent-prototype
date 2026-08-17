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
extended by entities from *successful* semantic tool observations (cap 24,
reset on user message / focus change). Failed execution results stay on the
TurnFrame (and may persist as typed `Error` items) but do not generic-heat
candidate paths. It overlaps `active entity match` for user-message
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
exceeds the send input budget (`provider context_window - output
reserve`, not the kernel pack cap). The engine proposes; the runtime
disposes. Pack and send are split: C materializes against
`min(kernel context_budget_tokens, send window)`; A may retain history
until the send guard trims.

Selection and consumption are separate operations:

```text
ContextEngine::materialize -> bounded, non-consuming preview + materialization_id
PromptAssembler/final guard -> exact provider frame
successful, non-stale ModelOutput
  -> ContextConsumptionAck(turn, operation, round, preview, item/ref ids)
  -> access reinforcement + bounded ContextConsumed event in one transaction
```

Previewed items removed by runtime packing receive no reinforcement. Refused,
failed, cancelled and stale operations send no acknowledgement. The simple
engine accepts only an exact subset of its one pending preview, validates all
owners before mutating any record, and stamps Resident/Warm bodies or External
descriptors without changing residency or semantic state. The runtime caps a
full-item acknowledgement at 256 ids and external descriptors at the
`ContextMapView` cap of 32. This makes GC recency/access signals describe
successful model use rather than candidate generation.

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
the message lands in a real task scope. Live runtime (`CTX-EVENT`,
2026-08-14) persists the exact body as a `user-input` artifact when a
workspace is wired, ingests the full `UserMessage`, then emits
`UserMessageAccepted` with a 240-char preview. `/focus` `/done` `/cancel`
stay direct commands. A second message while a turn is running occupies
one in-memory `Queued` slot and applies after that turn ends; a third
is `Rejected`. `/cancel` emits `InterruptCommitted` after `TurnCancelled`.
A successful turn publishes `Consumed` then `Archived` on the same input
id. Journal replay of `body_ref` traces needs a workspace. A `UserMessage` that still arrives
with no focus (engine used directly) is a session-level message: it falls
back to the session scope and stays selectable, but no focus is invented.
Later user messages update the current query while keeping the task goal. A
future task-boundary detector can replace this explicit/simple behavior.

Audit caveat (2026-08-10): the reference engine currently reuses one Focus
scope for the lifetime of a task. Every live member of that active scope is a
GC root, while same-task User/Assistant score floors stay above the archive
threshold. A sufficiently long task therefore trends back toward a resident,
selectable transcript even though final token packing is bounded. The next
policy step is an explicit Task → Episode/Focus model, not another threshold
tweak; see `docs/AUDIT_TODO.md` CTX-01 and its 10K-turn property test.

Since CTX-01 (closed 2026-08-10) the engine rotates the focus scope as an
**episode boundary**, so the working set tracks the current episode plus
unresolved semantic state instead of the whole task transcript:

```text
Session
  └─ Task (long-lived goal/authority)
       ├─ closed Episode/Focus 1
       ├─ closed Episode/Focus 2
       └─ current Episode/Focus
            └─ open Tool scopes
```

Two explainable signals rotate the episode, both configurable
(`episode_rotate_threshold`, `episode_max_user_turns`):

- a semantic boundary: a new user instruction that shares almost no tokens
  with the current episode's query AND carries real information (entities or
  length) — a bare continuation token does not rotate;
- the turn budget: even perfectly related instructions rotate once the
  episode exceeded `episode_max_user_turns` user turns.

Closing an episode promotes only durable outcomes (goal/constraint/decision/
finding/open-loop/artifact/evidence — the same `is_promotable` set as task
close) to the task scope and evicts ordinary dialogue. The GC then treats a
member of a closed scope as outside the working set: it is an eviction
candidate regardless of attention (the residency score floor no longer keeps
same-template dialogue Active forever), and it can be recalled only for a
fresh causal reason — a hot entity, a pin, or a model hint/lease — never for
the residency floor. The `long_task_10k_turns` property test keeps the
resident heap flat (~10 items) over 10,000 task turns while a durable
decision stays recallable and stale dialogue leaves Resident.

The episode turn budget is **episode-local**: rotation zeroes
`FocusState.generation`, so `episode_max_user_turns` measures the current
episode only. Without the reset, one overlong episode permanently exhausted
every later episode's budget (the guard fired on the very next user message
and rotated a fresh single-turn episode). `focus_generation` diagnostics
read the episode-local count.

The open episode is bounded too, not just the closed one. Related messages
share tokens, so the score floor keeps them Active forever and the
focus-scope root would otherwise accumulate every turn of a long episode
(a 500-turn episode held ~500 messages once the exhausted-budget rotation
stopped firing every message). GC ages **ordinary dialogue** out of the
heap — Working retention, no promotable outcome (the same
`is_promotable` set), not hot, not model-directed, older than the staleness
window residency uses (`ttl x 4`) — into the reversible buffer, and the
reactivate phase refuses to bounce it back on its own score. Only a hot
entity right now (a fresh causal reason) exempts it. So the resident
working set tracks the current episode's *active* activity plus unresolved
semantic state, not the whole episode transcript.

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
  runtime prepares the next model round (`ContextEngine::close_scope`). That
  close remains a scheduling heuristic; `ContextConsumed` is the exact record
  of which result ids a successful model operation actually received.
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

### Lifecycle metadata is authoritative across body locations (CTX-02)

Since the 2026-08-10 audit pass, terminal semantic transitions, task-close
protection cleanup and protection quotas no longer depend on where the
body currently sits (Resident / Warm / Stored):

- supersession, verification and recurrence scan the resident heap, the
  warm reversible buffer and the external map. A decision that was evicted
  and externalized is still the same decision: a later decision on the same
  entities supersedes it wherever it lives. `drain_supersessions` /
  `drain_verifications` apply the terminal state through
  `apply_terminal_semantic`, which refuses to re-transition a dead target
  (semantic transitions stay monotonic across every residency).
- keep-alive and lease quotas count heap + warm buffer together, so a
  warm-buffer protected item cannot bypass the cap.
- completing a task clears keep_alive/lease protections in the heap *and*
  the warm buffer, so a completed task cannot keep rooting items through an
  older record.
- automatic recall of a completed task's records is forbidden: the hot set
  alone cannot bring finished work back as current truth. GC roots, warm
  reactivation and cold-store recall all exclude completed-task items from
  the hot-entity path; only an explicit reason (pin / model hint / lease)
  re-admits them. (This also fixed a latent defect: after task completion
  the task's entities lingered in the hot set and kept the finished
  dialogue rooted forever.)

The catalog directory is a single `item_id -> location` record per id
(Resident / Warm / Stored) with shared query indexes (id / task / scope /
kind / entity / label / residency / attention). Authority metadata stays
on the body; GC moves location. `context.search` generates candidates from
those indexes across Resident/Warm/Stored; a live working-set file is a
catalog hit (heap projection), not an empty miss. A free-text needle that
hits no entity/label key still residual-scans summaries/uris/bodies. See
`docs/AUDIT_TODO.md` CTX-02.

### Retrieval results are transient; admit and derive move items deliberately (CTX-03)

The model-facing retrieval loop (`context.manage` op=search/inspect/fetch)
is **not an observation**: search/inspect may return catalog projections
(including Resident/Warm heap descriptors); fetch returns the catalog or
stored body. Catalog residency is not the selected working set.
Hits identify items with the catalog uri `context://run/<uuid>`; inspect /
fetch / admit / derive consume that same string (or the bare UUID).
`gc_hint` and `collect` are not model-facing ops — the engine owns
collection; a model call with those ops is an invalid request. The result is visible to the
current turn through the tool result, but
finalization must not persist it under a new `ToolObservation` id. Every
`TurnFrameStep::ToolResult` carries
a `ToolResultDisposition`:

- `PersistObservation` (default) — the result becomes a long-term
  observation at turn end.
- `TransientNoPersist` — search/inspect/fetch results, and the result text
  of a `derive` directive: visible to the turn, never persisted. The engine
  already stamps `last_access` on the read itself, so recency stays honest
  without duplicating evidence.
- `AccessEventOnly` — the result of an `admit` directive: not persisted as
  an observation, because the admission event *is* the record (the same
  item id must not be duplicated under a new id).

`fetch(ref)` is a pure read. `admit(ref, reason)` and `derive(ref, fact,
reason)` are explicit lifecycle moves:

- **admit** re-enters the item into the working set under its ORIGINAL id:
  heap-resident is a no-op; a warm-buffer item moves to the heap; an
  externalized item is read back from the store (plan -> io -> commit, the
  state lock is never held across disk IO) and re-stamped into the current
  working scope, producing exactly one `ContextStateTransition`
  ("admitted by model directive: <reason>"). The lifecycle clock is
  refreshed on admit — the item's presence in the heap is a fresh,
  deliberate act — so the ephemeral TTL does not tombstone it the moment it
  re-enters. Terminal semantic states refuse admit; stale ids are silent
  no-ops; the per-turn cap (`max_admits_per_turn`) bounds the model.
- **derive** mints a NEW item (`ContextKind::Note`) with an explicit
  `DependencyKind::DerivedFrom` edge to the source ref: the derivation is
  traceable but never confuses the source's identity with a copy. Bounded
  per turn (`max_derived_items_per_turn`) and per item (`max_item_chars`).

## 9c. P4: entity affinity and the explicit dependency graph

Two more explicit, non-learned signals make the working set follow what the
agent is actually touching.

### Entity affinity

The engine keeps a bounded **hot-entity set** (cap 24): seeded by the last
user message (`extract_entities(content)`), extended by entities appearing
in successful semantic tool observations and mid-turn `WorkingSetSignal`s
(most recent first), and reset by a new user message or `FocusChanged`.
Failed tool results (`ok: false` or a typed `failure_class`) do not extend
the set. An "entity" is a cheap signature — a whitespace token of
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
only along edges whose kind `requires_prompt_body()` (today:
`Continuation`). Auto-minted `SharesEntities` affinity and provenance
(`DerivedFrom`, `EvidenceFor`, `VerifiedBy`, `ArtifactOf`) do **not**
copy the target's body into the prompt:

- skipped: already-selected items, `Dropped`, and `superseded` /
  `verified-fixed` items (a verified error or superseded decision never
  re-enters through the back door);
- `Archived` continuations only when their score still clears the active
  threshold (same gate as primary selection — expansion never resurrects
  cold archived items);
- best continuations first (score desc), capped at +8 per snapshot;
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
- a bounded transitive slice of edges whose kind `requires_residency()`
  (`Continuation` today, cap `+8`), so a working item can keep the prior
  step of the same line of work. Weak affinity (`SharesEntities`) and
  provenance (`DerivedFrom` on a compact summary, `EvidenceFor`, …) do
  not mark or reactivate the target. The traversal follows
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
task. Roots are pinned items, the current focus scope, open loops,
durable task constraints, hot entities, explicit references, and the
**latest successful observation of each recent file path in the active
task** (capped at 8 paths; identified by a path-only first line as in
`fs.read`, not by a log that merely mentions a file). Same-path rereads
supersede the previous body (semantic death, so hot-entity recall cannot
bring stale file text back). A completed or switched-away task drops those
file-body roots, so pagination detail does not contaminate a later CSV
task. Plus a bounded `+8` transitive slice of their dependency edges.
Old turns of a long task therefore cool and evict like any other
working-set item.

### Eviction is reversible all the way down: ContextStore

The bounded eviction buffer no longer purges on overflow. The residency
ladder is:

```text
Resident -> Warm -> Cold -> External   (context GC)
External -> Storage GC -> Delete       (a future, separate pass)
```

The warm buffer shares the resident lifecycle clock, not just the ladder:
a live item that GC moved to the reversible buffer is still tombstoned by
the same windows residency uses — the ephemeral TTL, then the `ttl x 4`
staleness — on every maintenance pass, with pinned and keep-alive/lease
items exempt exactly like the resident root set. A tombstoned warm item is
never reactivated by hot entities, so a warm body cannot escape aging
forever (a consumed ephemeral observation used to sit in the buffer
reactivatable until overflow externalized it).

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

Since V1-M10 the actor also pushes the active task's **anchor root
projection** before every GC/Storage GC pass and inside every
materialization: `ContextAction::AnchorRoots { roots }` replaces the whole
projection (bounded by `MAX_ANCHOR_ROOT_CLAIMS`), and
`ContextHints.anchor_roots` carries the same projection on materialize.
The claims come from `TaskAnchor.working_refs` / `evidence_refs`
(`anchor_root_claims`), so task authority stays with the TaskManager — the
engine only ever sees a bounded projection, never the anchor. Semantics by
strength: `PromptRequired` forces the target into the model frame;
`ResidentRequired` protects (or recalls) the target in the working set —
GC marks it a root and reactivates it from the warm buffer or the cold
store; `StorageRequired` keeps the target's store entry out of Storage GC.
Claims resolve by item id, `context://run/<id>` uri, or exact entity
signature, and semantic death is terminal — a claim never resurrects a
superseded/verified-fixed/tombstoned item. The three strengths are
independent: `PromptRequired` is mandatory materialization, `ResidentRequired`
is online residency, `StorageRequired` is storage retention only. Each
projected claim carries `anchor_revision`, `source_field_id`, and a
`RootReason`; the GC and Storage GC reports list matching
`anchor_root_protections`. The completion boundary force-clears the
projection, so a finished task's records stop being rooted.

The same materialize request also carries a bounded `TaskAnchorView`
(`ContextHints.task` → `MaterializedContext.task`). That view is the
active task contract in the focus frame (`TASK ANCHOR rev=N`); the engine
copies it through without scoring it as a heap item. Goal/interpretation/
constraints/criteria/progress/open loops are in the view; working/evidence
refs stay on the root projection.

`CTX-11` is landed as an actor-owned `ResumePoint` bound to
`task_id + anchor_revision`. `TaskProgressView` is only its bounded prompt
projection. It records current objective, unresolved constraints/blockers,
next actions, checked file/entity refs with their observed digest/revision,
recent verification facts, and known failed-command facts. Bodies and full
command output remain in context/artifact storage. Updates land from trusted
tool results at durable turn commit; model prose is not authority. While
suspended, the view is absent from unrelated prompts. Reactivation
materializes `TaskAnchorView + TaskProgressView` from the runtime assembler;
it never reconstructs the old transcript.

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
so engines without a store remain compatible). The process service/wire
forwards all three operations, and parity tests force real externalization so
a missing override cannot pass by returning the trait default. Three
always-loaded
read-only meta-tools (`context.search` / `context.inspect` /
`context.fetch`) produce an `EngineQuery` the kernel resolves against the
engine (invariant 3 — tools still never touch the engine). As of
2026-08-15, search/inspect cover the live catalog (Resident/Warm
projections plus Stored), so a file still in SELECTED WORKING CONTEXT is
a hit with `residency=` as data, not an empty miss. `fetch_external`
stays a store read; a Resident/Warm id states that the body is already
in the working set. The prompt's `EXTERNAL CONTEXT` section shows refs
only (uri + id + kind/scope + residency + summary); `fetch` stamps recency
and the GC generation on the entry so ranking and Cold -> External aging
stay honest. A fetch is a deliberate read, not an automatic reactivation —
the model decides what re-enters the working set. The whole loop is
covered end to end by `agent-runtime/tests/recall.rs`: the model calls
`context.manage op=fetch` through a real runtime turn, the typed
`EngineQuery` is resolved by the kernel against the real engine and store,
and the exact content returns in the tool result — while the prompt's
external section carries only the bounded ref preview (content truncated
at externalization), never the full externalized content.

The engine-level fetch is still a read and does not reactivate the stored
record. Since CTX-03, the runtime also no longer persists retrieval results
as fresh observations: every `TurnFrameStep::ToolResult` carries a
`ToolResultDisposition` (`PersistObservation` | `TransientNoPersist` |
`AccessEventOnly`), and search/inspect/fetch results are transient — visible
to the turn, never duplicated under a new id. The explicit
fetch/admit/derive split (see §9b, "Retrieval results are transient...")
completes the retrieval surface: fetch is a pure read, admit re-enters an
item under its original id, derive mints a new derived item.

Default operational retrieval is Live-only at every entry point: materialized
external refs, search, inspect and fetch all hide Superseded, VerifiedFixed
and Tombstoned metadata. Fetch rechecks the canonical metadata after store IO
so a concurrent terminal transition cannot leak a body read that began while
the entry was Live. Terminal files remain available only to audit/Storage-GC
machinery, not as current model facts.

The materialized `external` field is a bounded `ContextMapView`, not a
clone of the whole map: at most 32 refs, quickselect-ranked in O(n)
without cloning it (hot-entity match first, then open-loop tags, then
recency, with a deterministic id tie-break). The output and cloned entry
count are bounded, but the current ranking still collects/scans O(total
external refs) temporary references; 100 K refs therefore do **not** cost the
same as 32. Indexed/streaming top-K work is tracked in `docs/AUDIT_TODO.md`
CTX-07.

External descriptors participate in the same consumption commit as inline
items. Merely previewing a ref does not update it; a successful acknowledged
frame stamps `last_access_tick`/`last_access_gc_epoch` without fetching or
reactivating the body. Retrieval access is graded (`CTX-GC-11`): a search
hit is the weakest signal (at most one Cold-aging delay, per-item cooldown,
and one identical-query stamp per turn), `inspect`/`fetch` are stronger
deliberate reads, `admit` is an explicit residency move, and the
consumption ack is the strongest online evidence. A weaker signal never
overwrites a stronger one. `fetch_external` remains a separate deliberate
read and records its own access.

Search returns bounded `ResourceDescriptor` cards (`CTX-DISC-01..03`). It
never admits a body or loads a tool; inspect/fetch/admit/load stay
explicit follow-ups. Misses distinguish `not_found`, `evidence_absent`,
and `provider_unavailable`. There is no public `runtime.search` surface.
`agent-eval` joins `ContextGcReport.externalized_ids` to later search
hits to report found-after-forgotten; that is instrumentation, not a
policy change.

Cold recall pre-filters in memory instead of reading the disk:
`ExternalizedContext` keeps the item's entity signature (`entities`),
`tags`, `dependencies`, `task_id` and the scope stamp (`scope_id`), so with
thousands of Cold entries only the entity-matching ids are read in the IO
phase (10 K refs -> a few reads), never the reverse. The `scope_id` stamp
is captured at externalize time so a scope close can re-stamp stored
entries' membership exactly like resident and warm bodies; entries
externalized before the stamp existed restore with `None` and fall back to
task-id matching on task/focus closes.

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

Because the state lock is released between the phases, the multi-phase
operations (GC, storage GC, store reconcile, checkpoint, restore) are
serialized by an engine-level operation gate: a plan computed against one
state can never be committed against a state a concurrent restore or
storage GC replaced in between. Single-phase operations (ingest, maintain,
materialize, ...) remain atomic under the state lock alone and never take
the gate — lock order is always gate, then state.

Storage GC is the only place information is permanently deleted, and it is
now a *strong-edge reachability closure* rather than a single
incoming-edge check. The dependency graph is typed, and the closure
distinguishes deliberate citations from weak affinity: `SharesEntities`
(the auto-minted entity-overlap link recorded at ingest) is never a
permanent-delete guard, while `EvidenceFor | DerivedFrom | VerifiedBy |
ArtifactOf | Continuation` are strong edges that keep their targets alive.
Roots are the strong-edge targets of resident/warm items **and every
non-deletable stored record itself** — a Live, Pinned or Durable record is
never a candidate, and its strong edges must keep its evidence targets
alive even when nothing resident references the record. From any
referenced record the closure traverses strong edges only, so
external -> external chains survive exactly when each hop is a strong
citation. `delete_file` returns
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

### 9i. V1-M9: the selection universe respects scope state

The materializer's candidate set previously admitted every scope belonging
to the active task — including already-*closed* tool frames — while the GC
mark phase correctly refused to cross a closed scope when walking a scope
chain. Those two rules disagreed, and the looser one let a closed tool
frame's observations re-enter the prompt purely because they carried the
active task's id. The candidate scopes now mirror the GC root rule:

- the session scope (always — scoring and the budget decide what reaches
  the frame);
- the active task's own task/focus scopes while they are open;
- tool scopes only while the frame is open.

A closed tool frame's observations leave the working-set lineage, exactly
like GC's closed-scope boundary. They are not lost — they re-enter through
the same channels every other cooled item uses: retention (a durable
outcome promoted on close), affinity (their entities are hot again) or an
explicit dependency edge. `closed_tool_scopes_are_not_candidates_but_hot_
entities_still_reach_them` pins both halves: the observation is absent
from the frame while nothing references it, and returns the moment its
entity is hot again.

### 9j. V1-M9: the external map owns its indexes (ExternalMap)

The heap is not the only index owner anymore. `State.external` was a bare
`Vec<ExternalizedContext>` mutated directly by every external path; now
that retrieval and recall read id/entity indexes, the map binds storage
and indexes together like `ContextHeap`: `push` (externalize commit),
`retain` (recall removes), `take_all` / `replace_all` (storage-GC commit,
restore) go through methods that rebuild the id + exact-entity indexes in
the same step, while non-indexed fields (residency aging, access stamps)
stay reachable via `&mut` iteration. `inspect_external` and the
`fetch_external` membership/stamp path are O(1) id lookups instead of
linear scans; the GC recall pass reads exact hot-entity matches from the
entity buckets and keeps a residual scan for substring-tolerant overlaps
(hot `AuthService.rs` vs an entry entity `src/auth/AuthService.rs`) so
recall coverage is unchanged. Checkpoints serialize only the entries;
indexes rebuild on restore.

The materialized `external` field is the bounded `ContextMapView` named in
§9g, and since P1 the cap is enforced by the *type*, not just the
selection code: the constructor asserts `<= 32` and the wire
deserializer rejects over-cap payloads, so the bound holds on both sides
of the context-service boundary.

### 9k. V1-M9: typed dependency edges and the scope tree index

The dependency graph is typed. `ContextItem.dependencies` (and the
externalized entry's captured edges, which the Storage GC reachability
closure reads) are now `Vec<DependencyEdge>` — `{ target, kind }` —
instead of bare ids, so GC reachability and supersession/evidence
policies can distinguish *why* an item is referenced. The kind taxonomy
separates consumers, not just strong vs weak:

```text
                ranking   prompt body   residency   storage
SharesEntities  yes       no            no          no
DerivedFrom     no        no            no          yes
EvidenceFor     yes       no            no          yes
VerifiedBy      no        no            no          yes
ArtifactOf      no        no            no          yes
Continuation    yes       yes           yes         yes
```

`is_strong()` remains the storage-citation alias (`protects_storage()`).
Prompt expansion uses `requires_prompt_body()`; full-GC mark/reactivate
uses `requires_residency()`. Affinity is not a citation; a citation is
not prompt inclusion; prompt inclusion is not residency; residency is not
storage reachability. `SharesEntities` (the entity-overlap link recorded
at ingest, new -> prior) ranks and links. `DerivedFrom` (minted by
`context.derive` and episode compaction) is provenance so a compact card
can find its sources later — it must not pull those sources back into the
prompt or resurrect them Warm → Resident. The wire deserializer accepts
the pre-typed bare-id form, so checkpoints written before the graph was
typed keep loading.
`ContextItemSummary.dependencies` remains a projection of target ids
only, so replay and the UI are untouched.

The scope tree owns its id index too. `State.scopes` is a `ScopeTree`
(bare `Vec` + id index): scope ids are immutable, so `push` is the only
structural mutation and close/ancestor lookups (`close_scope`,
`nearest_open_parent`, the `belongs_to` parent walk) are O(1) `by_id` /
`index_of` instead of per-hop linear scans. Checkpoints serialize only
the scopes; the index rebuilds on restore.

### 9l. Store blob ownership and crash recovery

Every formal blob (`<id>.json` under the store dir) has exactly one owner:
the external-map entry whose `ExternalizedContext.blob_checksum` is the
FNV-1a content hash captured at write time (corruption/bit-rot detection
for the reconcile; the hot read path skips it so per-item retrieval stays
IO-cheap). Ownership holds across every state transition:

- **Externalize** pre-serializes the bytes under the lock and keeps the
  source item with the caller (`GcPlan::externalize` carries `(item,
  bytes)`, id-keyed); the spawned IO task only writes. A `JoinError`
  (panic/cancellation) therefore returns every unconsumed item to the
  buffer — the source item is never lost with its task. Writes are
  atomic: temp file -> flush + sync -> rename, so a crash mid-write
  leaves only a `.tmp` file, never a half-written blob under the formal
  name. Store IO (writes, recall reads, post-commit deletes) is bounded
  by `MAX_STORE_IO_CONCURRENCY = 8` via a shared semaphore.
- **Recall** re-enters the content as a resident item and removes the map
  entry at commit; the blob is deleted only *after* the commit (phase 4 of
  `gc()`, outside the lock, with per-id IO errors surfaced in
  `ContextGcReport.store_blob_delete_errors`). A crash between commit and
  delete leaves a stale blob whose id is resident — the reconcile
  reclaims it; content is never lost.
- **Startup reconcile** (`ContextEngine::reconcile_store()`, default
  empty report; `context-simple` implements it) converges the directory
  with the map under the same plan/io/commit split as GC, so the state
  lock is never held across disk IO. Each blob is classified: a valid
  ownerless blob is rebuilt into an entry (context GC never purges — a
  reachable file becomes a reference again); a blob whose id is resident
  is reclaimed as a stale duplicate; an unreadable / id-mismatched /
  checksum-mismatched blob is moved to `quarantine/` (evidence preserved,
  never guessed away); an abandoned `.tmp` file is removed; a real IO
  error leaves the blob in place and is surfaced. `StoreReconcileReport`
  counts every bucket and explains each action. The service boundary
  forwards it (`ServiceOp::ReconcileStore`), with parity tests proving the
  wire op, the adapter override and the sidecar handling agree with the
  in-process engine.

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
- which model turns consumed it (exact ids recorded by `ContextConsumed`;
  legacy traces without the event retain the old `ContextPrepared` behavior);
- whether it was later reactivated (`Archived/Cooling -> Active` transitions).

### 11.1 What the journal records per item

- **Entry**: item `created_turn` plus `source` (user / assistant / tool:<name> /
  explicit-pin / task-summary) and kind.
- **Selection preview**: every `ContextPrepared` event carries the final
  post-packing selected items with `score`, `approx_tokens`, and a `ScoreBreakdown`
  (`importance`, `focus`, `recency`, `access`, `scope_bonus`, `retention_bonus`,
  `total`). It does not itself claim successful consumption.
- **Consumption commit**: `ContextConsumed` carries the bounded
  `ContextConsumptionAck` with turn/operation/model-round/materialization
  identities plus exact full-item and external-ref ids. Only this event's
  transaction bumps `access_count`/`last_selected_turn`. Search/inspect/fetch
  may stamp a weaker recency/`gc_epoch` clock (`CTX-GC-11`) without counting
  as consumption. No ack means no consumption reinforcement.
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
the runtime made, then maps each recorded final selection onto the fresh
engine and commits it only when the matching `ContextConsumed` arrives
(maintenance triggers are recorded on `ContextMaintained` events). Output is
a per-item lifecycle report plus final diagnostics. Cost and fact-coverage
passes use independent fresh engines, so one measurement cannot seed the
other's state.

```bash
cargo run -p agent-replay -- .focus-agent/traces/<run>.jsonl
```

Replay is deterministic for a given engine version: the recorded transitions
are ground truth, and replay re-derives the same story from the events.

### 11.3 Checkpoints vs event history

`/checkpoint` (or `ContextEngine::checkpoint`) exports the engine's runtime
state — items, focus, tick/turn counters — to a separate JSON file under
`.focus-agent/checkpoints/`, independent of the event journal. `restore`
replaces engine state from such a file, and before the restored state
becomes live it runs a structural validation pass: duplicate ids inside one
location, an id owned by more than one location, scope ancestry and item
scope references must all hold, or the restore is refused with an explicit
error. Store-file existence is not checked here — the startup reconcile owns
blob recovery. This keeps durable runtime state separate from the
append-only trace used for learning/replay.

## 12. Turn Frame vs Context Frame (V1)

Since V1-M1 the model input is assembled in five layers, and tool
observations flow in two distinct phases:

- **During a turn** tool results live in the runtime-owned `TurnFrame` (the
  execution stack): they are rendered as protocol messages — an assistant
  message carrying `tool_calls`, then a `tool` message paired by
  `tool_call_id` — and they are never scored, garbage-collected or evicted.
  The current user message, current TaskAnchor/Focus, and current tool
  result are runtime-owned. `PromptAssembler` receives runtime Focus and
  TaskAnchor plus historical `MaterializedContext`; engines (A, B, and C)
  must not mint a Goal from `FocusChanged` or select a `UserMessage`
  whose `created_turn` is the current user-turn clock. Catalog residency
  is not the selected working set.
- **When the turn ends** (the model stops calling tools) the observations
  are persisted as the long-term record: `ingest(ToolObservation)` for each
  result, then one `maintain(AfterTool)` pass, then the final assistant
  message and `maintain(AfterModel)`. Live `fs.read` stamps workspace
  `path` and content `revision` on `ToolOutput.metadata`; ingest copies
  those onto `ContextItem.file_path` / `file_revision` and the entity
  catalog. The numbered `model_content` is not a path header. Latest-file-
  body roots and path search use the structured identity (replay
  `path:\nbody` remains a fallback). Writes and patches are not file-body
  observations even if they later carry a path.

Consequences:

- no mid-turn duplication: the model sees each result once, in protocol
  form, and the context frame does not re-render the same observation;
- long-term memory still accumulates every observation (errors, verified
  fixes, decisions) — as a batch at turn end, so the error/supersession
  lifecycle observes the whole turn together;
- `AfterTool` maintenance now runs once per turn (at persist time) instead
  of once per tool call. The replay/A/B/C harnesses drive the engines
  directly and are unaffected by the kernel's timing.

### 12.1 The turn end is a durable commit (V1-M10)

Persisting the turn is not best-effort cleanup: `finalize_turn` walks
`Running → ModelFinished → Committing → Committed`, and every mandatory
write (observation ingest, `AfterTool`/`AfterModel` maintenance, the full
GC, and their journal events) must succeed before `TurnCompleted` is
emitted. On the first failure the commit aborts and the runtime journals
`TurnCommitFailed { phase, message }` + `RecoveryRequired` — the model
answered, but the context frame was not durably updated. This is what makes
the long-term record trustworthy: "the model said X" and "the context frame
reflects X" can only diverge when an explicit recovery-required signal says
so, never silently.

`TurnCompleted` itself is published through the kernel's `emit_event_durable`
path: append to the event journal, then flush it, then broadcast. Because
the journal channel is FIFO, a successful flush is an OS-level durability
barrier covering every event appended before it — the subscriber never sees
a committed turn unless all mandatory state writes have left the process. A
failed barrier broadcasts nothing and routes to `TurnCommitFailed`; ordinary
events (not turn boundaries) are still append-only and flush on stop, so the
hot path stays off the persistence path.
