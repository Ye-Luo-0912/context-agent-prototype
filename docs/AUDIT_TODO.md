# Whole-project audit follow-up

Audit date: 2026-08-10

This is the hand-off queue from the whole-project audit. It separates
confirmed correctness/security defects from context-policy decisions that
must not be hidden behind another scoring tweak.

Rules for follow-up agents:

- Preserve the dependency and context invariants in `AGENTS.md`.
- M12/M13 blockers below must close before Self-Iteration is enabled.
- Do not add a database, vector search or learned ranking. The target remains
  an explainable, non-vector baseline.
- Every context change needs a failing regression/property test and bounded
  runtime events. Full lifecycle detail belongs in JSONL/artifacts.
- Do not claim a milestone complete because happy-path tests pass; test the
  named failure point in each acceptance criterion.

## Closed in the 2026-08-10 repair pass

- [x] Workspace missing-prefix alias (`missing/file` resolving to a root
  `file`) and pre-planted external `.focus-agent` link.
- [x] `git.diff` confinement and option injection; the direct Git child now
  observes cancellation and `kill_on_drop`.
- [x] Focus/suspend/complete context transitions use checkpoint rollback;
  task state commits only after context ingest + maintenance succeeds.
- [x] Journal failure after focus no longer splits task and context
  authority; it becomes an explicit recovery-required audit gap and fences
  further mutation.
- [x] Restore validates `tasks.active`, `current_task_id`, the active row and
  restored context focus before exposing task state. A failed actor restore
  no longer changes capability flags. Rollback failure fences mutations until
  a known-good full restore succeeds.
- [x] Context-service process parity includes `search_external`,
  `inspect_external` and `fetch_external`; tests force real externalization.
- [x] Terminal external entries are hidden from materialized refs, search,
  inspect and fetch, including a post-I/O fetch recheck.
- [x] Pre-epoch external entries receive a real `gc_epoch` anchor and age.
- [x] The actor enforces a 16,000-character last-line guard on every
  model-facing tool result before TurnFrame/context/event admission.

## P0 — blocks the context goal or trusted execution

### CTX-01 — Long tasks collapse back into an append-only transcript

Decision gate: discuss Task/Focus/Episode semantics before implementation.
**Closed 2026-08-10** — see the CONTEXT_LIFECYCLE "Episode boundary" note.

Implemented:

- `SimpleContextConfig.episode_rotate_threshold` (default 0.15) and
  `episode_max_user_turns` (default 500);
- `scope.rs::close_focus_episode` rotates the focus scope on the semantic
  boundary (token overlap below the threshold AND informative content) or
  the turn budget; the close promotes durable outcomes to the task scope and
  evicts ordinary dialogue (close_members now evicts for Focus too);
- `gc/full.rs`: a member of a closed scope is an eviction candidate
  regardless of attention (the residency score floor can no longer keep
  same-template dialogue Active forever), and is recallable only for a fresh
  causal reason (hot entity / pin / model hint/lease), never for the score
  floor;
- ingest-applied rotation transitions surface in the next maintenance report
  (`pending_ingest_transitions`), so the rotation is observable.

Acceptance: `long_task_10k_turns_keeps_the_working_set_episode_bounded`
(10,000 turns, mixed semantic/related workloads): resident heap stays flat
(~10 items, peak < 200, no growth between turn 2,000 and 10,000), the
durable decision stays recallable via hot entities, and stale ordinary
dialogue leaves Resident. Confirmed the failure mode the fix targets: before
the closed-scope GC rule, same-template messages kept a focus-match floor
above `active_threshold`, were rated Active every maintain pass, and were
never evictable.

Residual correctness follow-up (does not invalidate the bounded-residency
acceptance): episode rotation closes the Focus scope but does not reset the
shared `FocusState.generation`. Once the 500-turn guard first fires, every
later user message can satisfy the same guard and rotate again. Reset or
replace the episode-local counter and add a cadence regression proving one
overlong episode does not permanently exhaust every later episode's budget.

Confirmed behavior (as audited):

- `context-simple/src/scope.rs::open_focus_scope` reused one Focus scope for
  the entire task;
- every User/Assistant message is `Working` inside that long-lived focus;
- same-task score floors remain above the default archive threshold after
  recency vanishes (User ≈ 0.349, Assistant ≈ 0.296, threshold 0.24);
- `gc/full.rs::mark_roots` roots every live member of the active Focus scope,
  regardless of `Active`/`Cooling` attention;
- the materializer admits all `Cooling` candidates and runtime supplies no
  finite `max_selected_items`.

Resident bytes and candidate work therefore grew approximately with `2 ×
task turns`; token packing hid prompt overflow but not the growing heap or
selection cost. That linear growth is now bounded by episode rotation.

Recommended design:

```text
Session
  └─ Task (long-lived goal/authority)
       ├─ closed Episode/Focus 1
       ├─ closed Episode/Focus 2
       └─ current Episode/Focus
            └─ open Tool scopes
```

Rotate an episode on a new user instruction, explicit phase/subgoal change,
or runtime boundary signal. Closing an episode promotes only goal,
constraint, decision, finding, open-loop and artifact/evidence outcomes.
Current episode + explicit durable/leased records + strong causal evidence
form roots; task membership alone never does.

Minimum experiment:

- `long_task_10k_turns`: Resident bytes and candidate count are bounded by
  current episode + unresolved semantic state, not turn count;
- required constraints/decisions remain recallable;
- stale ordinary dialogue leaves Resident;
- task success does not regress versus the current engine.

### CTX-02 — Lifecycle metadata is not authoritative across residency

Semantic transitions, task close, scope promotion, quotas, lease expiry,
supersession, verification and recurrence mostly operate only on Resident.
Directives can mutate Warm while quotas count only Resident. Warm/Cold facts
can retain completed-task leases or return as Live after a newer fact already
superseded them.
**Closed 2026-08-10 (short-term behavior fixes; the ContextCatalog
restructure stays the structural target below).**

Implemented:

- supersession, verification and recurrence now reach every body location:
  `drain_supersessions` / `drain_verifications` apply terminal semantics to
  the resident heap, the warm buffer, or the stored entry via
  `apply_terminal_semantic`; `queue_decision_supersessions`,
  `queue_error_verifications` and `queue_error_recurrence` scan all three
  locations. A stored decision is superseded by a later decision on the
  same entities; a warm error is verified by a later success and superseded
  by a recurring failure.
- keep-alive/lease accounting is global: `apply_directive` counts leased
  items/tokens and keep-alive items across heap + warm buffer.
- a completed task clears keep_alive/lease protections in every residency
  (heap + warm buffer).
- automatic recall of a completed task's records is forbidden: GC roots,
  warm reactivation and cold-store recall all exclude completed-task items
  from the hot-entity path. Only an explicit reason (pin / model hint /
  lease) brings finished work back.
- semantic transitions remain monotonic through every body location
  (`apply_terminal_semantic` refuses to re-transition a dead target).

Acceptance (new property tests): `supersession_reaches_warm_and_stored_
decisions`, `verification_reaches_warm_errors`, `recurrence_supersedes_
warm_errors`, `completed_task_clears_protections_in_every_residency`,
`completed_task_blocks_automatic_hot_recall`, `keep_alive_quota_counts_
warm_items`. The hot-root test exposed a real defect: completed-task
entities lingering in the hot set kept the finished dialogue rooted; GC
roots now exclude completed tasks from the hot path.

Correct structural target:

```text
ContextCatalog
  item_id -> ContextRecord
             identity / task / scope
             attention / semantic / retention
             entities / tags / protections / typed edges
             body_location = Resident | Warm | Stored(ContextRef)
```

GC moves `body_location`; it does not move or duplicate authority metadata.
Property tests must run supersession, recurrence, verification, completion
and quota sequences with the target initially in every residency.

Residual correctness follow-up: terminal semantic mutations now span body
locations, but minor TTL/tombstone aging still iterates Resident items and
Stored metadata cannot represent every keep/lease directive field. A live
item that moves to Warm/Cold/External can therefore escape the same lifecycle
clock. Make aging/protection semantics catalog-owned before closing the
structural target.

### CTX-03 — Fetch/Search/Inspect persist as new observations

The contract says fetch is a transient store read, but runtime tool results
enter TurnFrame and later become a new ToolObservation with a new item id.
Search/inspect results are persisted too.

Add a result disposition:

```text
PersistObservation | TransientNoPersist | AccessEventOnly
```

`context.search`, `context.inspect` and `context.fetch` should be transient.
Separate `fetch(ref)` (current TurnFrame only), `admit(ref, reason)` (same item
id) and `derive(ref, fact, reason)` (new id + `DerivedFrom`).

Acceptance: fetch/search/inspect leave catalog ids/count unchanged; admit
preserves identity and produces one lifecycle transition.

**Closed 2026-08-10.**

Implemented:

- `ToolResultDisposition` is now three-valued — `PersistObservation`
  (default) | `TransientNoPersist` | `AccessEventOnly` — and rides on
  `TurnFrameStep::ToolResult`. The actor marks `EngineQuery` results
  (search/inspect/fetch) `TransientNoPersist`, `context.admit` results
  `AccessEventOnly` (the admission event is the record — the same item id
  must not be duplicated), and `context.derive` results
  `TransientNoPersist` (the derived item is the record). `finalize_turn`
  skips every non-`PersistObservation` step, so a transient read never
  becomes a `ToolObservation` with a new id.
- `context.manage` gains `admit` and `derive` ops alongside
  search/inspect/fetch. `admit(ref, reason)` routes a
  `ContextAction::Admit` that re-enters the item into the working set under
  its ORIGINAL id — heap resident is a no-op, warm buffer moves to the
  heap, and an externalized item is read back from the store (plan -> io ->
  commit, the lock is never held across disk IO) — with exactly one
  `ContextStateTransition` ("admitted by model directive: <reason>") and a
  refreshed lifecycle clock (the explicitly admitted item is a fresh
  working-set member, so the ephemeral TTL does not tombstone it the moment
  it re-enters). Terminal semantic states refuse admit ("terminal states
  never resurrect"); stale ids are silent no-ops.
- `derive(ref, fact, reason)` routes a `ContextAction::Derive` that mints a
  NEW item (`ContextKind::Note`) with an explicit `DependencyKind::DerivedFrom`
  edge to the source ref — traceable, never a copy of the source id.
- both directives are quota-bounded per turn (`max_admits_per_turn`,
  `max_derived_items_per_turn`, default 8 each); content is bounded by
  `max_item_chars`.

Acceptance (new tests): engine-level
`admit_externalized_item_preserves_identity_and_produces_one_transition`,
`admit_warm_buffer_item_preserves_identity_and_one_transition`,
`admit_refused_for_terminal_semantic_item`,
`admit_and_derive_respect_per_turn_quotas`,
`derive_creates_a_new_item_with_a_derived_from_edge`; runtime-level
`recall_turn_pulls_external_content_back_without_polluting_the_prompt`
(fetch + search + inspect in one turn leave catalog count unchanged) and
`admit_and_derive_through_the_runtime_never_duplicate_observations`
(catalog delta is exactly the turn's messages + the admitted item + the
derived Note; no directive result is duplicated).

### CTX-04 — Context-store blob ownership and crash recovery are incomplete

Confirmed defects:

- recall removes the external-map owner but leaves the file, making an orphan
  Storage GC cannot discover;
- externalization removes an item from the buffer before async IO/commit;
  crash, cancellation or `JoinError` can lose owner and/or item;
- store files have no startup reconcile, revision/checksum validation or
  bounded IO concurrency.

Required work:

- delete a blob only after successful recall commit, or retain its
  `ContextRef` on the canonical record;
- temp write -> flush/sync -> atomic rename;
- checkpointable `InFlightExternalize` state or durable manifest entry;
- restore/start reconcile for pending, orphan, missing, corrupt and
  id-mismatch blobs; quarantine uncertainty rather than ignoring it;
- recover the source item on every `JoinError` and cap IO concurrency.

Inject crashes after temp write, rename, map commit, map removal and delete.
After restart every formal blob has one owner and every Stored record has one
readable blob; orphan/dangling counts are zero.

**Closed 2026-08-10.**

Implemented:

- Every formal blob has exactly one owner. The external-map entry now
  carries the checksum captured at write time
  (`ExternalizedContext.blob_checksum`), so the startup reconcile can prove
  a blob belongs to its record. A recalled item's blob is deleted only
  AFTER the recall commit lands (`gc()` phase 4, outside the lock): a crash
  between commit and delete leaves an orphan the reconcile re-owns, never
  lost content.
- Externalization no longer detaches the source item into the IO task:
  `GcPlan::externalize` keeps `(item, pre-serialized bytes)` with the
  caller, so a `JoinError` (panic/cancellation) returns every unconsumed
  item to the buffer instead of losing it with its task. Writes are
  atomic: temp file -> flush + sync -> rename, so a crash mid-write leaves
  only a `.tmp` file, never a half-written blob.
- Store IO is bounded: `MAX_STORE_IO_CONCURRENCY = 8` via a shared
  `Arc<Semaphore>` across externalize writes, recall reads and post-commit
  blob deletes.
- `ContextEngine::reconcile_store()` is a new contract method (default
  returns an empty report; `context-simple` implements it) that converges
  the on-disk directory with the external map under the same plan/io/commit
  split as GC: the lock is never held across disk IO. Valid ownerless
  blobs are rebuilt into entries (context GC never purges — a reachable
  file becomes a reference again); blobs whose id is resident are reclaimed
  as stale duplicates; unreadable / id-mismatched / checksum-mismatched
  blobs are moved to `quarantine/` (evidence preserved, never guessed
  away); abandoned `.tmp` files are removed; real IO errors are surfaced
  per blob and left in place. The full report (`StoreReconcileReport`)
  counts every bucket and explains each action.
- The service boundary is closed: `ServiceOp::ReconcileStore` crosses the
  wire with a snake_case tag, the adapter overrides `reconcile_store`, and
  the sidecar dispatches it — verified by
  `reconcile_store_parity_between_in_process_and_service_boundary`.

Acceptance (new tests): store-level
`reconcile_heals_each_crash_window_without_losing_ownership` (temp write /
rename / healthy / recall-delete windows),
`reconcile_classifies_every_blob_state_in_one_pass`,
`reconcile_leaves_exactly_one_owner_per_blob` (orphan/dangling counts
zero, every entry has a readable blob matching its checksum),
`reconcile_quarantines_a_tampered_blob_against_its_entry_checksum`;
engine-level `reconcile_store_converges_a_crash_injected_directory`;
service-boundary parity `reconcile_store_parity_between_in_process_and_
service_boundary`. The pre-existing
`gc_externalizes_overflow_and_recalls_via_the_store` assertion flipped to
the new ownership rule: recalled blobs are removed once their content is
resident.

Residual follow-up: quarantine preserves the map entry whose blob was moved
aside (the report explains it, an operator can restore it), and the storage
GC strong-edge traversal is the next store-safety item (CTX-05).

### CTX-05 — Storage GC may delete evidence referenced by live stored facts

Roots start from Resident/Warm outgoing targets. A Live/Pinned/Durable stored
record that is not itself referenced does not contribute its dependencies,
so its terminal evidence target may be permanently deleted.

First distinguish strong edges from weak affinity. `SharesEntities` is not a
permanent-delete guard. Candidate strong edges:

```text
EvidenceFor | DerivedFrom | VerifiedBy | ArtifactOf | Continuation
```

Storage GC must root every non-deletable record and traverse strong edges
before selecting a terminal, retention-eligible, expired target. Add random
graph safety and liveness tests across all body locations.

### CTX-10 — TaskAnchor and completion output have no authoritative contract

The runtime can recycle completed-task process detail, but it cannot identify
or durably protect the actual task result:

- `TaskRecord`/RuntimeCheckpoint only retain id, goal, status and timestamps;
  suspended work has no authoritative criteria, plan, open loops or resume
  point;
- the model's actual final response is an ordinary Working
  `AssistantMessage`, is truncated before entering ContextItem, and is
  archived with the task's ordinary dialogue;
- `/done <summary>` accepts unrelated free text and writes it as a Session
  Durable Summary after focus was cleared, so it gets `task_id=None`, becomes
  a global GC/materialization root, and is not linked to final output,
  artifacts, acceptance criteria or verification;
- `TaskCompleted` events omit task/result identity, and restore cannot verify
  that a completed task owns exactly one committed outcome.

Required contract:

- actor-owned bounded/versioned `TaskAnchor` with goal, user-authority
  constraints, acceptance criteria, plan progress, current episode, open
  loops and typed root claims; updates are sourced CAS patches;
- separate Prompt, Resident and Storage root semantics. The Anchor itself is
  task authority, not a scored ContextItem;
- immutable `CompletionRecord`/`TaskOutcome` with task id, anchor revision,
  outcome status, exact final-output body ref + digest, acceptance results,
  artifacts/effects, verification, unresolved state and episode outcomes;
- atomic root transfer: `CompletionPrepared` first protects/finalizes output
  and evidence, then commits the outcome, closes scopes, releases active
  roots and finally commits TaskManager completion. Failure remains
  Active/Completing/ClosePending and is idempotently recoverable;
- a completed outcome is a Storage root and explicit task-catalog result, not
  an automatic prompt/residency root for unrelated tasks.

Acceptance:

- `Completed` iff one committed CompletionRecord exists and task/focus scopes
  are closed; restore rejects every other combination;
- final output is byte-for-byte readable by digest after overflow, restart
  and Storage GC;
- completion fault injection never produces a half-closed task or releases
  the only output/evidence roots;
- 1,000 completed tasks keep Resident/candidate work bounded, while every
  outcome remains searchable by task id;
- Active -> Suspend -> unrelated GC -> Resume restores Anchor revision,
  criteria/open loops and ResumePoint without replaying the old transcript.

### CORE-01 — Process capabilities bypass effect and approval boundaries

Affected: `agent-capability-process`, `agent-process`, registration/approval.

Confirmed chain:

- a process mutates inside the child and returns only `ToolOutput`; the
  adapter wraps it as `CapabilityOutcome::Value`, bypassing actor generation,
  cancel and effect rollback;
- a side-effecting process tool can self-declare `ReadOnly`, which approval
  auto-allows;
- cwd + env filtering + Unix rlimits are not filesystem/network isolation;
  absolute FS/network remain available and Windows has no equivalent rlimit;
- `manifest.id` enters a predictable temp path without strict path-safe id
  validation;
- inherited stderr is unbounded.

External process capabilities must remain disabled until this closes; M12/
M13 are not implemented trust boundaries yet.

Required order:

1. conservative manifest-id grammar and private unpredictable directories;
2. derive minimum risk/permissions from transport and requested authority;
3. replace child-performed mutation with brokered `EffectRequest`;
4. broker FS/network/process access and deny undeclared access;
5. process group / Windows Job Object cancellation, including descendants;
6. bounded stdout/stderr + artifact spill and disk/CPU/memory/process quotas;
7. adversarial tests proving a cancelled/ReadOnly process cannot mutate an
   absolute path, use undeclared network or outlive its operation.

### CORE-02 — Event-journal enqueue is not a durable turn commit

`FileEventJournal::append` acknowledges channel enqueue. Background open/
write/serialize errors stay in `last_error` until `flush`, while finalization
marks `Committed` without a flush barrier; kernel normally flushes on stop.

Required contract:

- explicitly choose writer ack, OS flush or fsync as turn durability;
- add a sequence/barrier ack and make writer errors sticky immediately;
- publish `TurnCompleted` only after mandatory state and that barrier;
- fault tests: trace path removed/denied after startup, disk full, background
  writer failure, and crash immediately after commit.

Keep persistence buffered/off the hot path; a barrier need not fsync every
ordinary event.

## P1 — confirmed defects and hardening

### CTX-06 — Full GC/storage operation semantics

- external-only state may skip full GC, preventing aging/recall;
- a marked Warm/Cold dependency is not necessarily recalled because mark,
  sweep and reactivation use different universes;
- plan/IO/commit lacks an operation mutex or revision/CAS: concurrent GC,
  storage GC, restore and checkpoint can commit stale plans;
- restore migration validates Resident only, not duplicate ids, scope
  ancestry, body location or store files;
- task close may leave deep descendants open;
- scope-close promotion/transition logic primarily visits Resident bodies;
- tool-scope close currently discards context-close errors and returned
  transitions instead of publishing an auditable result;
- a named task summary can inherit the wrong current focus identity/scope.

First safe step: serialize GC/storage-GC/checkpoint/restore and validate all
layers. Longer term use `context_revision + base_revision` CAS.

### CTX-07 — Materializer budget and hot-path correctness

- dependency reserve is taken when expansion is disabled/impossible;
- score top-K happens before fit packing, so an oversized top item can hide a
  lower-ranked item that fits;
- external preview has item cap but no token cap and is omitted from
  `approx_tokens`; runtime trimming removes items, not refs;
- exact indexes and substring entity matching disagree;
- external preview and session/focus candidates remain O(total history),
  despite documentation claiming constant cost.

Add packing properties and candidate-count/materialize-p95 metrics. A finite
runtime `max_selected_items` is defense in depth, not a replacement for
CTX-01.

### CTX-08 — Mid-turn tool discoveries do not update recall signals

Tool bodies correctly remain in TurnFrame until finalization, but a file or
symbol discovered by a tool is not hot before the next model round. Add a
bounded, no-body `WorkingSetSignal` at tool commit, then persist the body only
at final commit.

Acceptance: discovering `AuthService.rs` can make the very next model round
recall Warm/Cold evidence without duplicating the tool body.

### CTX-09 — Lifecycle clocks and observability need explicit semantics

`tick` advances on ingest, maintain, GC and materialize, yet several TTLs
interpret it as age. Separate `event_seq`, `user_turn`, `gc_epoch` and
`last_selected_turn`; every rule names its clock.

Use bounded GC event counters/samples plus an artifact-backed ledger with
item/revision/axis/from/to/cause/trigger/turn/related-id. Diagnostics must
cover all body locations; root/externalize/age/recall need item reasons.
`ContextDiagnostics.total_items` and `inspect()` currently count Resident
only, so replay `final_total` is not a logical-catalog total. Also propagate
audit failures from `BeforeModel` maintenance and explicit collect; a state
change must not silently outrun its journal event.

### CORE-03 — Checkpoint capture is not an atomic cross-plane snapshot

Restore ordering/rollback is fixed, but capture awaits actor state then reads
the shared capability registry without a freeze. Concurrent surface mutation
can produce a mixed snapshot. Public `RuntimeHandle::checkpoint/restore`
also omits host capability state.

Move capability surface under one actor-owned snapshot protocol or add a
shared freeze/generation handshake. Make partial APIs crate-private or name
them explicitly actor-only.

### CORE-04 — Output broker/resource policy is incomplete

The actor hard limit protects model context, but a producer without an
artifact loses the truncated middle. Process responses may allocate to the
frame limit, metadata/summary can be large, and inherited stderr bypasses it.

Add a trusted output broker before `ToolOutcome` reaches the actor: store
once, return bounded preview/reference, cap every field and decoded total,
and apply it to context fetch/provider errors. Enforce query limits in
execution, not only JSON schema.

### CORE-05 — Untrusted historical content is promoted to System role

`PromptAssembler` renders selected historical user/tool/file content and
external summaries inside `ModelMessage::system`, giving retrieved prompt
injection system precedence.

Keep policy/instructions in System; render observations in a delimited
lower-authority role/structured field. Add malicious file/tool/store evals.

### CORE-06 — Cancellation/approval/process cleanup

- Git now kills the direct child, not guaranteed descendants;
- shell/process capabilities need process groups/Job Objects and bounded
  artifact/line/total-output quotas;
- approval timeout/cancel needs pending cleanup and bounded previews; UI
  defaults must not turn ambiguity/truncation into allow;
- provider streams need total response/error/SSE byte caps and explicit
  cancellation at EOF/backoff boundaries.

### CORE-07 — Workspace operations remain TOCTOU-sensitive

Canonicalization blocks known pre-planted escapes, but validation and later
open/create can race a link swap. Trusted Core should use directory-handle-
relative/openat-style operations (and Windows equivalent), reject reparse
substitution at operation time, and test concurrent swaps.

### CORE-08 — Per-call approval cannot support unattended long tasks

The current policy has two extremes:

- `PolicyApprovalGate` uses global booleans for all WorkspaceWrite and all
  ProcessExecution calls;
- `InteractiveApprovalGate` prompts on every non-read-only call, waits up to
  five minutes, then denies on timeout.

There is no task/session-scoped standing grant, target/path restriction,
effect/reversibility classification, expiry/revocation, interruption budget,
batched boundary request, or “deny this effect and continue independent work”
contract. Coarse `ProcessExecution` also treats a bounded local test and a
dangerous external command as the same interaction burden.

Required direction (after/unified with M12 Effect Runtime and M14 Resource
Policy):

- trusted `TaskExecutionPolicy` with narrow effect + target + constraint +
  expiry grants; the model can use but never widen it;
- automatic operation inside the standing sandbox/task grant;
- derive risk from the prepared effect, target scope and reversibility, not
  only a tool's declaration;
- no responder means deny/skip and continue safe independent work, never
  implicit allow and never an indefinitely blocked run;
- aggregate unavoidable goal/scope/irreversible boundary choices at a
  checkpoint with an interruption cap;
- finish `Partial`/`Blocked` with one consolidated boundary report when an
  ungranted effect is truly essential.

Acceptance: a long coding task can edit its granted workspace and run bounded
local tests without per-call prompts; it cannot push/deploy/delete broadly or
access secrets/network without the matching narrow grant; zero user responses
produces no privilege expansion and no five-minute-per-call stall.

### CORE-09 — Tool schema budget mutates lifecycle and can forget required capability

The model-round snapshot is correctly bounded, but the final input guard in
`RuntimeActor` responds to fixed-layer pressure by calling `tool_unload` on
the largest schemas. That permanently changes catalog lifecycle state because
one provider round had a small input budget. No TaskAnchor/Focus root or
Required-vs-Preferred semantics prevents a task-critical editor/test tool
from being unloaded. Dynamic capabilities also use one owner-level
`loaded: bool`, so loading one tool exposes every sibling schema and they do
not receive builtin idle cooling.

Required direction:

- separate catalog/authority, operational lifecycle, and one-round surface;
- token/schema budget performs pure round-local packing and never unloads a
  tool or changes its lifecycle;
- actor supplies typed TaskAnchor/Focus/Active-call roots at the existing
  BeforeModel safe point;
- `MustSurface` tools are selected or produce explicit
  `ToolSurfaceUnsatisfiable`; `PreferSurface` omissions are observable but do
  not mutate lifecycle; `KeepReady` tools stay cheap to reactivate without
  entering every prompt;
- make external capability lifecycle per tool while process start/stop remains
  owner-level; loading one tool must not expose all siblings;
- replace generation arithmetic with one monotonic surface revision covering
  catalog, Anchor, Focus, and execution-policy revisions;
- checkpoint authority/Anchor requirements and durable leases, not a derived
  per-round surface or `Active` state.

Acceptance: shrinking then restoring a provider budget produces identical
catalog lifecycle; every rooted required tool appears or the round fails with
an explicit reason; optional omission never bumps catalog generation; one
capability tool does not surface siblings; quarantine after snapshot still
revokes execution; suspend/resume reconstructs the surface from TaskAnchor
without transcript replay.

## P2 — policy quality and evaluation

- terminal semantic checks should precede pinned retention;
- replace weak `SharesEntities` pseudo-dependencies with typed edges;
- add store corruption/reconcile and lifecycle growth-slope metrics;
- make fact comparison replay each policy on a fresh engine; the current
  observing run and coverage run reuse one engine, contaminating the latter;
- replace the fixed-marker rolling baseline with real compaction and account
  for actor, compactor, recall, store, tool-schema and wall-time cost;
- audit process parity whenever `ContextEngine` gains a method;
- compare dynamic, rolling and append-only engines on these scenarios:

```text
long_task_10k_turns
decision_superseded_while_warm
decision_superseded_while_cold
error_verified_after_externalization
tool_discovers_entity_mid_turn
task_A_to_B_to_A
checkpoint_during_gc_failure
store_orphan_recovery
causal_evidence_in_cold_store
retrieved_prompt_injection
```

Primary acceptance statement:

```text
The working set grows with the current episode and unresolved semantic state,
not linearly with task conversation turns. Required facts are recalled for a
new causal reason; terminal facts never return as current truth; GC/store keep
one owner through failure and recovery; task success is not traded for token
savings.
```

## Suggested independent Agent work packages

1. **Task authority/completion:** CTX-10 contracts, checkpoint, root transfer
   and fault tests; do not combine with scoring changes.
2. **Context properties:** residency × lifecycle tests for CTX-01/02/03
   before policy changes.
3. **Store integrity:** CTX-04/05 plus crash injection/reconcile; no scoring
   edits.
4. **Context concurrency:** CTX-06 with operation gate/restore validation.
5. **Materializer:** CTX-07/08 and budget/candidate metrics; no store edits.
6. **Trusted effect + sandbox:** CORE-01/06/07/08 in M12 -> M14 order.
7. **Durability:** CORE-02 plus recovery replay, isolated from policy.
8. **Prompt/resource boundary:** CORE-04/05 with adversarial evals.

Do not run packages editing the same crate concurrently unless file ownership
is explicitly partitioned.
