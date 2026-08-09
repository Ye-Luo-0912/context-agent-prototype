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

## P2 — policy quality and evaluation

- terminal semantic checks should precede pinned retention;
- replace weak `SharesEntities` pseudo-dependencies with typed edges;
- add store corruption/reconcile and lifecycle growth-slope metrics;
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

1. **Context properties:** residency × lifecycle tests for CTX-01/02/03
   before policy changes.
2. **Store integrity:** CTX-04/05 plus crash injection/reconcile; no scoring
   edits.
3. **Context concurrency:** CTX-06 with operation gate/restore validation.
4. **Materializer:** CTX-07/08 and budget/candidate metrics; no store edits.
5. **Trusted effect + sandbox:** CORE-01/06/07 in M12 -> M13 order.
6. **Durability:** CORE-02 plus recovery replay, isolated from policy.
7. **Prompt/resource boundary:** CORE-04/05 with adversarial evals.

Do not run packages editing the same crate concurrently unless file ownership
is explicitly partitioned.
