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
- [x] Materialization is a non-consuming preview. A successful non-stale
  model result commits the exact post-packing inline/external ids through a
  bounded `ContextConsumptionAck`; trim/refusal/failure/cancel/stale paths do
  not reinforce, and event-append failure rolls the access mutation back.
- [x] Replay cost and fact-coverage observations use independent fresh engine
  instances; a regression compares the aggregate result with a standalone
  fresh coverage run.

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

**Closed 2026-08-10.**

Implemented:

- `DependencyKind` grows the strong-edge taxonomy — `EvidenceFor`,
  `VerifiedBy`, `ArtifactOf`, `Continuation` alongside `DerivedFrom` —
  and `DependencyKind::is_strong()` draws the line: `SharesEntities`
  (auto-minted entity overlap at ingest) is weak affinity and is never a
  permanent-delete guard; the other kinds are deliberate citations.
- `plan_storage_gc` is now a strong-edge closure. Roots are the
  strong-edge targets of resident/warm items **and every non-deletable
  stored record itself** (Live semantic, or Pinned/Durable retention) —
  so a Live stored decision citing a terminal evidence file keeps that
  file alive even when nothing resident references the decision. From any
  referenced record the closure traverses strong edges only, so
  external -> external chains survive exactly when each hop is a strong
  citation; a stored record's entity overlap with a terminal neighbor
  never pins it.

Acceptance (new tests): `storage_gc_roots_live_stored_records_through_
strong_edges` (the reported defect: a Live record's strong edge protects
its terminal evidence; its weak edge does not),
`weak_shares_edges_never_protect_from_permanent_deletion` (resident weak
edge and stored weak edge both fail to pin),
`storage_gc_reaches_through_external_dependency_chains` updated to strong
edges at every hop, and the seeded random-graph property test
`storage_gc_strong_edge_closure_matches_manual_reachability` (60 entries
with random semantics/retention/edge kinds + resident roots): the plan
must equal a manually computed strong-edge closure, and the delete pass
must leave exactly the closure survivors alive.

### CTX-10 — TaskAnchor and completion output have no authoritative contract

**DONE.** The runtime now owns the authoritative task contract end to end:

- **Actor-owned `TaskAnchor`** (`b7a1330`). Every `TaskRecord` carries a
  bounded, versioned anchor: original goal, current interpretation,
  constraints, acceptance criteria, plan progress, open loops, and typed
  root claims (`ContextRootClaim` with role + strength, split into
  `working_refs` residency claims and `evidence_refs` retention claims).
  The whole anchor is replaced through compare-and-swap (equivalent anchors
  idempotent, completed tasks immutable, every field capped by the
  `MAX_TASK_ANCHOR_*` bounds), a bounded `TaskAnchorChanged` audit event
  names only the moved fields, `RuntimeCheckpoint` is version 3 and
  persists the anchor, and restore validates its bounds and revision
  semantics. The anchor is task authority, never a scored ContextItem.
- **Immutable typed `CompletionRecord`** (`73110ca`). Completing a task
  commits exactly one outcome — task id, anchor revision, bounded summary,
  optional final-output ref, and bounded artifact refs — atomically with
  the status flip in the `TaskManager`. `TaskCompleted` events carry
  task/result identity. Restore rejects any checkpoint where a Completed
  task lacks a record, a record names an open/unknown task, or the record's
  anchor revision disagrees with the task anchor.
- **Atomic root transfer** (`7699672`). The completion transaction keeps
  its ordering — the context engine records the completed task and closes
  its scopes first (rollback on failure), then the `TaskManager` commits
  status + outcome. Fault injection proves no half-closed task: a refused
  completion ingest leaves the task Active with the active slot intact, and
  a journal that refuses the typed completion event keeps the aligned
  committed state, marks recovery-required, emits `RecoveryRequired`, and
  fences checkpoint/mutation until a known-good restore.
- **Storage roots, not residency roots** (`4ef0798`). A completed task's
  records were unconditional GC roots (durable session memory), so the
  resident heap grew linearly with the task count. Mark now excludes
  completed-task session records from that root rule, the reactivation
  score fallback applies the completed-task guard the hot-entity path
  already had, and the actor runs one full GC pass after a completion
  commits (publishing the `ContextGc` report). Acceptance: 1,000 completed
  tasks stay bounded in the resident heap while every outcome stays
  searchable by task id.
- **Verifiable final output** (`110fd6c`). Each `CompletionRecord` carries
  a deterministic final-output ref (`task:<id>:completion`) and the
  SHA-256 digest of the exact final output body, so the outcome stays
  byte-for-byte verifiable after overflow, restart and Storage GC.

Regressions: `task_anchor_update_publishes_a_bounded_event`,
`task_anchor_survives_checkpoint_restore`,
`completion_commits_a_typed_record_and_publishes_task_identity`,
`restore_rejects_completed_task_without_a_completion_record`,
`completion_failure_never_leaves_a_half_closed_task`,
`completion_audit_gap_marks_recovery_but_keeps_the_commit`,
`thousand_completed_tasks_stay_bounded_and_searchable`,
`suspend_and_resume_preserves_anchor_without_replaying_transcript`,
`completion_record_carries_a_verifiable_final_output_digest`,
`completed_task_summary_leaves_the_resident_heap_but_stays_durable`.
**CTX-10 closed.**

### CORE-01 — Process capabilities bypass effect and approval boundaries

Affected: `agent-capability-process`, `agent-process`, registration/approval.

Confirmed chain:

- a process mutates inside the child and returns only `ToolOutput`; the
  adapter wraps it as `CapabilityOutcome::Value`, bypassing actor generation,
  cancel and effect rollback;
- an in-process capability granted `workspace:write` can call
  `WorkspaceHandle::write`, which applies the mutation immediately during
  `invoke`; only the sibling `prepare_write` path reaches a staged Effect;
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
3. remove/refuse direct capability `WorkspaceHandle::write` and replace
   child/in-process mutation with brokered `EffectRequest`;
4. broker FS/network/process access and deny undeclared access;
5. process group / Windows Job Object cancellation, including descendants;
6. bounded stdout/stderr + artifact spill and disk/CPU/memory/process quotas;
7. adversarial tests proving a cancelled/ReadOnly process cannot mutate an
   absolute path, use undeclared network or outlive its operation.

**Partially repaired 2026-08-10; wire broker landed 2026-08-11.** The
in-process/runtime-owned mutation path is fenced, process mutations now
cross a brokered wire effect path, and external process capabilities remain
Disabled by default. The OS isolation defects below keep CORE-01/M12/M13
open.

Implemented:

1. **Manifest-id grammar + private directories** — `validate_capability_id`
   (`agent-contracts`): lowercase/digit start, `[a-z0-9._-]`, <= 64 chars,
   enforced at registration before anything derived from the id, and again
   in the adapter's `from_manifest`. The capability working directory is
   `temp_dir()/context-agent-capability-<id>-<uuid>` — unpredictable, so
   two runs never share a path and a hostile pre-created directory cannot
   be predicted or symlink-trapped.
2. **Risk/permissions derived, never self-declared** —
   `validate_manifest_authority` at registration (and mirrored in the
   adapter): unknown permission strings are refused (`is_known_permission`:
   `workspace:read | workspace:write | process:run | runtime:context-control
   | artifact:*`); a capability declaring workspace-write/process-run may
   not mark any tool `ReadOnly` (ReadOnly auto-allows at the approval
   gate); a `WorkspaceWrite` tool needs `workspace:write`, a
   `ProcessExecution` tool needs `process:run`; a process-transport
   capability may declare `workspace:write` because the wire effect broker
   (item 8 below) stages its mutations.
3. **No direct capability mutation** — the runtime hands a
   `workspace:write` capability a `StagedOnlyWorkspace` handle whose
   `write` is refused ("must be staged") and whose `prepare_write` returns
   an `Effect` committed by the core behind the generation fence
   (`CapabilityOutcome::EffectRequest`), exactly like a builtin tool. A
   mutation can no longer land during `invoke`.
4. **Undeclared access denied by construction** — the invocation context is
   built from declared permissions alone (no declared workspace permission
   -> no handle; `workspace:read` -> `ReadOnlyWorkspace` with both write
   paths blocked), and now the registry refuses unknown permission strings
   up front.
5. **Process-tree cancellation** — already enforced (Unix process group +
   SIGKILL, Windows `taskkill /T /F`) and verified by the existing
   heartbeat test; cancellation kills the whole tree, not just the child.
6. **Bounded stderr** — `ProcessSandbox::stderr_capture_bytes`: when set
   (the capability sandbox uses 64 KiB), the child's stderr is piped and
   drained by a task into a bounded ring; the tail is exposed via
   `ProcessHost::stderr_tail()` for diagnostics. A chatty child can no
   longer inherit unbounded output into the parent console.
7. **Adversarial tests** — `capability_authority_is_derived_and_validated_
   at_registration` (path-unsafe id, self-declared ReadOnly,
   over-granted tool risk, process `workspace:write` now registers through
   the wire broker, read-only process allowed), `undeclared_permissions_
   receive_no_handle` updated (direct write refused, staged write commits,
   unknown permission refused at registration),
   `from_manifest_rejects_ids_that_could_escape_a_path`,
   `from_manifest_rejects_readonly_tools_on_write_capabilities`,
   `private_capability_dirs_are_unpredictable_and_path_safe`, and
   `stderr_is_drained_into_a_bounded_tail` (a 4 MiB stderr flood leaves an
   8 KiB tail ending in the newest bytes).
8. **Wire-level effect broker** — `WireEffect` (`agent-contracts`): a
   process capability declares structured mutation intent over the invoke
   wire (`workspace_write` with a base64 payload, so arbitrary bytes cross
   JSON safely) instead of mutating inside the child. The adapter's
   `stage_wire_effects` (`agent-capability-process`) validates every effect
   against the invocation's granted permissions and stages it through the
   confined workspace handle (`prepare_write`), then returns
   `CapabilityOutcome::EffectRequest` — the core commits the composite
   effect behind the generation fence exactly like a builtin tool's
   `PreparedEffect`. A plain `ToolOutput` response still decodes as a
   no-effect `Value` (backward compatible); `Vec<Box<dyn Effect>>` commits
   sub-effects in order and stops at the first failure. The
   registration-time refusal of process `workspace:write` is lifted. Wire
   tests: `wire_effect_round_trips_binary_content_over_json`,
   `composite_effect_commits_every_sub_effect_in_order` /
   `composite_effect_stops_at_the_first_failure` /
   `composite_effect_rolls_back_every_sub_effect`,
   `staged_wire_write_returns_an_effect_request` (nothing lands until the
   runtime commits), `wire_write_without_the_grant_is_refused` and
   `wire_write_without_a_workspace_handle_is_refused`.

Residual (M12/M13; CORE-01 remains open): OS-level filesystem/network isolation
for the child process (absolute paths and network remain available to a
hostile child at the OS layer) and Windows Job-Object quota enforcement.
The wire-level effect broker is implemented: a child stages structured
mutations as `EffectRequest`s and the core commits them behind the
generation fence.

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

**Closed 2026-08-10 (durability choice = OS flush barrier, not fsync).**

Implemented:

1. **Durability contract: the OS flush barrier** — `FileEventJournal::flush`
   is the turn-commit barrier. The command channel is FIFO, so a successful
   `flush` guarantees every event appended before it has been drained by the
   blocking writer and flushed out of each `BufWriter` to the OS; events
   still sitting in userspace buffers or the pipe are not durable until the
   barrier passes. The async hot path still only enqueues (`append`), so
   persistence stays off the hot path and ordinary events are not fsync'd.
2. **Sticky writer errors** — the first failed write poisons the writer:
   `failed` is set once and never cleared; every later barrier reports that
   same error instead of pretending the trace is intact; appends after the
   failure are still drained from the channel (the event sequence stays
   consistent) but dropped; the final channel-close flush is skipped when
   already failed.
3. **`TurnCompleted` published only after the barrier** — the kernel gains
   `emit_event_durable` (append → flush → broadcast; a failed barrier
   broadcasts nothing), and the actor's `finalize_turn` uses it for
   `TurnCompleted`. A subscriber never sees a committed turn unless every
   mandatory state write before it (tool observations, assistant message,
   maintains, GC) has left the process. A failed barrier routes to
   `commit_failed(TurnCommitPhase::TurnCompletedEvent)`: the runtime emits
   `TurnCommitFailed` + `RecoveryRequired` and drops the turn frame instead
   of claiming a commit that never landed.
4. **Fault tests** — agent-storage: `flush_is_a_durability_barrier_over_
   prior_appends`, `writer_errors_are_sticky_at_the_next_barrier` (a
   directory squatting on a trace path makes the writer's open fail on every
   platform — is-a-directory on unix, access-denied on windows), and
   `events_are_not_durable_until_the_barrier` (crash-immediately-after-
   commit shape: the buffered tail is invisible on disk until the barrier,
   the flushed prefix survives). agent-runtime actor tests:
   `turn_completed_is_broadcast_only_after_the_barrier` (TurnCompleted is
   the last event appended before the flush, and the barrier has passed by
   the time the broadcast is observed) and
   `failed_barrier_blocks_turn_completed_and_marks_recovery_required` (no
   TurnCompleted broadcast, `TurnCommitFailed` phase `turn_completed_event`,
   `RecoveryRequired` emitted, turn frame dropped).

Acceptance: a subscriber observes `TurnCompleted` only after a flush barrier
has covered every mandatory state write; a failed barrier surfaces
`TurnCommitFailed`/`RecoveryRequired` and never broadcasts `TurnCompleted`.

Residual (not closed here): crash-recovery replay that re-reads the trace to
rebuild runtime state after a barrier failure — this closes the barrier
contract itself, not the recovery machinery; a genuinely full disk is covered
by the sticky-error path but not exercised with a real full volume.

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

**First safe step (serialization) closed 2026-08-10.** The engine now owns an
operation gate that serializes the multi-phase/whole-state operations — GC,
storage GC, store reconcile, checkpoint and restore. Each of those spans
several state-lock acquisitions (the state lock is deliberately released
across disk IO), so without the gate a plan computed against one state could
be committed against a state a concurrent restore/storage-GC replaced in
between. The gate is acquired before the state lock in every gated operation
(consistent lock order, no deadlock), and single-phase operations (ingest,
maintain, materialize, acknowledge, scope/search/fetch/inspect) are untouched
— they stay atomic under the state lock alone and never take the gate.
Observability is unchanged: the operations' existing bounded events
(`ContextGc`, the storage-GC wire report) remain the runtime surface;
serialization itself is a structural guarantee, proven by
`multi_phase_operations_are_serialized_by_the_operation_gate` (each of the
five operations blocks while the gate is held and completes after release —
a regression that fails the moment any gated operation stops waiting).

**Restore layer validation closed 2026-08-10.** `restore` now runs a
structural validation pass after deserialization and refuses a violating
checkpoint with an explicit error instead of adopting it. The checks are the
invariants the engine maintains at runtime, all in-memory and O(total
ids/scopes): a duplicate id inside one location (the heap id index hides
these with last-wins, so the raw vectors are scanned), an id owned by more
than one of heap / eviction buffer / external map, a scope whose parent is
missing from the tree, and an item whose scope reference is missing. A valid
round-trip is unchanged; corrupt/hostile checkpoints are rejected by
`restore_rejects_checkpoints_that_violate_structural_invariants`. Store-file
existence is deliberately left to the startup reconcile (which owns blob
recovery), not to restore.

**Mark/reactivate universe agreement closed 2026-08-10.** The mark phase's
dependency traversal now resolves edges across every residency — the heap,
the warm buffer and the external map (entries capture their edges at
externalize time) — and the reactivate phase honors those marks: a Warm
buffer item or Cold store entry that a live root depends on is recalled as
"dependency of a marked root", regardless of closed-scope/completed-task
guards (the root is live right now) and even when no hot entity names it.
Previously the traversal only followed edges through the heap and
reactivation only recalled hot-entity/score matches, so a demoted
dependency was marked but never brought back. Regression:
`demoted_dependency_is_recalled_because_a_live_root_depends_on_it` (a low-
score, non-hot warm-buffer evidence item is recalled because a pinned live
decision cites it) plus `dependency_edges_resolve_across_heap_buffer_and_store`.

**External-only state no longer skips the GC pass closed 2026-08-10.** The
pass-skip check treated an empty heap and eviction buffer as "nothing to
do", so a state whose items were all externalized never ran a full GC pass
again: Cold entries never aged to External (aging only happens when a pass
increments `gc_epoch`) and recall never got the chance to run. The check now
requires the external map to be empty too; an external-only state runs the
pass so Cold aging and recall keep working. Regression:
`external_only_state_still_ages_cold_entries_on_full_gc` (a Cold entry
ages to External in one pass when nothing is resident).

**Scope-close promotion for warm bodies closed 2026-08-10.** The
scope-close promotion pass only visited the resident heap, so a durable
outcome that happened to be evicted to the warm buffer before the scope
closed lost its promotion — it kept pointing at the closed scope. The close
pass now promotes warm-buffer members of the closing scope exactly like
heap members (terminal semantics and excluded items stay out), and a
promoted item re-enters the heap: promotion means resident, not just
re-stamped. Regression:
`warm_buffer_durable_outcome_is_promoted_on_scope_close` (a Durable
decision in the buffer is promoted to the task scope, labeled, and becomes
resident again when the focus episode closes).

**External scope-close promotion signal closed 2026-08-10.** External
entries (Cold/External) previously carried only `task_id`, so a scope close
could not tell whether a stored entry belonged to the closing scope — a
durable outcome that had already left the engine lost its membership
promotion and kept pointing at the closed scope. `ExternalizedContext` now
captures the `scope_id` stamp at externalize time (legacy entries restore
with `None` and fall back to task-id matching, which is the safe
approximation for the whole task line), and the scope-close pass promotes
stored entries exactly like resident and warm bodies: scope/scope_id
re-stamp to the nearest open ancestor, retention upgrades to durable, the
move is labeled, and attention moves to Active like a resident promotion —
with the same no-op guard (already a member of the promotion target) and
the same terminal-semantics exclusion. The promotion re-stamps the
identity; the content stays in the store. Regression:
`external_durable_outcome_is_promoted_on_scope_close` (a stored Durable
decision promotes to the task scope on focus close and to the session on
task close, while a Working body and another task's entry stay untouched).
The membership walk now uses the scope tree's O(1) id index instead of a
linear scan, so the close pass stays O(items) even when the tree holds many
closed scopes.

**Task-close descendant handling closed 2026-08-10.** The task-close queue
collected only the task scope and its direct focus child, so a tool frame
nested under the focus stayed open — still Active and pointing at scopes
that were already closed. `queue_task_scope_close` now walks the scope tree
depth-first and queues every open descendant (focus episodes and the tool
frames inside them); each queued close promotes durable outcomes to the
nearest open ancestor, so the deepest tool frame's outcome lands in the
session once task and focus are closed. Regression:
`task_close_closes_deep_descendants_and_promotes_their_outcomes` (a
four-level session/task/focus/tool tree: every scope closes and the tool
frame's Durable decision is promoted, labeled and observable).

**Tool-scope close error publishing closed 2026-08-10.** The runtime's
`close_tool_frames` discarded both the close error and the returned
transitions (`let _ = context_close_scope(...)`), so a failed tool-frame
close was silent and the promotions it produced were never observable. The
close is now an auditable result: a successful close publishes a
`ToolScopeClosed` event carrying the transitions (durable outcomes promoted
out of the frame); a failed close publishes an `Error` naming the scope and
the failure. The TUI renders the transitions in the same lifecycle panel as
every other transition. Regressions:
`tool_scope_close_publishes_its_transitions` (the event carries the
transitions the engine returned and the closed scope id matches) and
`tool_scope_close_failure_is_published_as_an_error` (a failing close
surfaces an `Error` naming the close instead of being swallowed).

**Task-summary focus identity closed 2026-08-10.** The summary item built
for a `TaskCompleted` ingest inherited whatever identity happened to be
active at build time: the focus was cleared before the item was made (so an
unnamed completion lost its task id) and a *named* completion arriving
while another task was focused stamped the summary with the focused task's
id and scope. The summary now belongs to the completed task line: its
task id and scope (the completed task's open task scope, or the session as
a fallback) are captured before the focus/close machinery runs and the item
is re-stamped after `make_item`, so a summary never inherits the current
focus identity. Regressions:
`named_task_summary_does_not_inherit_the_current_focus` (A completes while
B is focused: the summary carries A's id and scope and B's focus stays
untouched) and `unnamed_task_summary_keeps_the_focused_tasks_identity` (an
unnamed completion keeps the focused task's id even though the focus is
cleared while the item is built).

CTX-06 closed.

### CTX-07 — Materializer budget and hot-path correctness

**Partially repaired 2026-08-10.** The false-consumption subissue is closed:

- `materialize` returns a monotonic, non-consuming preview and records the
  bounded ids eligible for one acknowledgement;
- the actor's final frame produces
  `ContextConsumptionAck { turn_id, operation_id, model_round,
  materialization_id, item_ids, external_item_ids }` only after a successful
  non-stale model operation;
- `context-simple` validates the exact preview subset and all residency owners
  before atomically stamping access; it does not reactivate external bodies;
- the kernel couples reinforcement to the bounded `ContextConsumed` event
  with checkpoint rollback, and replay commits only on that event (legacy
  traces retain explicit compatibility behavior);
- actor trim, refusal, cancellation/stale output, journal failure, external
  descriptor, invalid-retry, replay and service-parity tests cover the path.

**Budget and hot-path repair closed 2026-08-10.** The five remaining
materializer subissues are closed:

- **Dependency reserve** is only carved out when `dependency_expansion` is
  enabled — with expansion disabled the whole budget belongs to the working
  set instead of a reserve that is never spent. Regression:
  `dependency_reserve_is_not_taken_when_expansion_is_disabled`.
- **Top-K no longer precedes fit packing.** The candidate list is no longer
  pre-trimmed to `max_selected_items` before the budget pack: an oversized
  top item that cannot fit no longer hides a lower-ranked item that does
  (packing's own cap checks enforce the bound). Regression:
  `oversized_top_item_does_not_hide_a_lower_ranked_item_that_fits`.
- **External refs are token-capped and charged.** The ranked ref walk stops
  at a 512-token summary bound (uri + summary), and `approx_tokens` now
  includes the refs' cost — refs are model-visible, not free. Regression:
  `external_refs_are_token_capped_and_charged`.
- **Candidate generation and scoring share one matching universe.** The
  exact entity index cannot express a substring overlap
  (`src/auth/AuthService.rs` vs a hot `AuthService.rs`), so the materializer
  runs a bounded residual pass over the GC-bounded heap to bring such items
  into the candidate set — the scorer's substring affinity can finally fire
  for them. Regression: `substring_entity_match_reaches_the_candidate_universe`.
- **The external ref view no longer walks the whole map.** Hot-entity
  matches come from the entity index (O(bucket) per hot entity) and the
  rest is the most-recently-externalized tail (the map stores in
  externalize order, so the tail is a bounded O(1) recency approximation),
  keeping the view independent of total history. Regression:
  `external_view_surfaces_hot_matches_beyond_the_recency_tail`.

CTX-07 closed.

Add packing properties and candidate-count/materialize-p95 metrics. A finite
runtime `max_selected_items` and ack cap are defense in depth, not a
replacement for CTX-01 or bounded candidate work.

### CTX-08 — Mid-turn tool discoveries do not update recall signals

Tool bodies correctly remain in TurnFrame until finalization, but a file or
symbol discovered by a tool is not hot before the next model round. Add a
bounded, no-body `WorkingSetSignal` at tool commit, then persist the body only
at final commit.

Acceptance: discovering `AuthService.rs` can make the very next model round
recall Warm/Cold evidence without duplicating the tool body.

**Closed 2026-08-10.** The actor now emits a bounded, no-body
`ContextIngress::WorkingSetSignal` when a tool result commits to the turn
frame: the engine merges the entities extracted from the tool's bounded
output into the hot set immediately, so the very next model round's
candidate generation and scoring see them (recall of cooled evidence needs
a hot entity to fire). No item is created and nothing is persisted — the
observation body still lands only at turn-end finalization, so the tool
body is never duplicated. Regressions:
`working_set_signal_extends_hot_entities_without_creating_a_body` (the
engine merges the signaled entities with no item created) and
`tool_commit_signals_discovered_entities_before_the_next_round` (the actor
sends the signal at tool commit, before the turn-end observation ingest;
the observed ingest order in the turn test now shows `WorkingSetSignal`
between the user message and the persisted tool observation). The existing
recall end-to-end test now exercises the acceptance directly: a fetch tool
that discovers `AuthService.rs` signals it, and the turn-boundary GC
recalls the seeded Warm/Cold entries into the resident heap under their
original ids — while the fetch/search/inspect results themselves are never
persisted as new ToolObservations.

### CTX-09 — Lifecycle clocks and observability need explicit semantics

DONE (clocks) — `event_seq` (monotonic event sequence, advanced by every
state-changing operation, never by `materialize`), `turn` (user-turn clock,
advanced once per user message), `gc_epoch` (full-GC generation, pre-existing)
and `last_selected_turn` (stamped on consumption acknowledgement) are
separate clocks and every rule names its. Ephemeral TTL and staleness age
in user turns (`created_turn`), the consumed-ephemeral check uses event
distance (`created_tick`), and recency scoring reads `last_selected_turn` —
so a preview is a read that advances no clock and ages no item, and an event
burst inside one user turn cannot force TTL death. `ContextDiagnostics`
exposes `event_seq`; `alias = "tick"` keeps pre-separation checkpoints
loadable. Regressions:
`materialize_preview_is_a_read_that_advances_no_clock`,
`selection_stamp_is_written_only_by_consumption_ack`,
`ephemeral_ttl_counts_user_turns_not_events`. The replay heavy-scenario
saving threshold moved from >50% to >60% because a single turn's flood of
irrelevant output is now compressed by consumed-archive + generational
eviction rather than TTL.

DONE (catalog) — `ContextDiagnostics.total_items` and `inspect()` are the
logical catalog (resident heap + warm eviction buffer + external store
entries; each id has exactly one owner, so the sum is exact) and replay
`final_total` is a real catalog total. External entries project into
`inspect()` from their store descriptor (`external_summary`:
`externalized_at_tick` as `created_tick`, `source = "externalized"`), so
fetch/admit/reactivate is a location move that never changes the total;
the fetch/admit regression tests assert the moved entry still projects as
`externalized`.

DONE (ledger) — the bounded, artifact-backed lifecycle ledger records every
item transition on any axis (`attention` / `semantic` / `residency` / `gc`)
with item/revision/axis/from/to/cause/trigger/turn/related-id and the
event-sequence clock, projected where transitions already exist (residency
pass, scope close, supersession/verification, GC eviction/reactivation/
externalization/recall, admit/derive directives). The buffer is capped
(`max_ledger_records`, oldest rows drop, per-item revisions stay
monotonic), rides the checkpoint (restore keeps it), and `export_ledger`
writes a JSONL artifact off the hot path (temp file + rename; export
drains the buffer). Diagnostics already cover all body locations
(resident/warm/cold/external) and root/externalize/age/recall carry item
reasons. Regressions: `lifecycle_ledger_records_maintenance_and_gc_rows`,
`lifecycle_ledger_survives_checkpoint`, plus ledger unit tests for
boundedness, per-item revisions and checkpoint round-trip.

DONE (audit propagation) — a state change no longer outruns its journal
event: a failed `ContextMaintained` (BeforeModel) publication fences the
turn (Error event, model never called, no `TurnCompleted`), and an
explicit `collect` propagates both a refused GC pass and a failed
`ContextGc` publication as `Error` events; the turn-boundary GC audit
fault already fences the commit into `RecoveryRequired`. Regressions:
`before_model_audit_failure_fences_the_turn`,
`collect_audit_failure_is_not_silent`. **CTX-09 closed.**

### CORE-03 — Checkpoint capture is not an atomic cross-plane snapshot

**DONE.** Capture is now an atomic cross-plane snapshot via a
freeze/generation handshake: `RuntimeInstance::checkpoint` reads the
capability registry generation, captures the actor state, snapshots the
registry, and retries (bounded) whenever the generation moved — a still-
moving surface returns `AgentError::Internal` instead of a mixed snapshot.
The public `RuntimeInstance::checkpoint/restore` also carries host capability
state; a rejected actor restore leaves activation/load flags untouched.

Live restore now publishes one bounded restore-commit audit event,
`RuntimeEvent::RuntimeRestored`, carrying checkpoint version, restored and
current run ids, old/restored/effective focus and surface revisions
(`RestoreRevision`), the rebased task-requirement count plus a capped 16-id
sample (artifact spill reserved for full detail), and whether capability
state was applied. If appending this mandatory barrier fails after the
context + task authority commit, the restored aligned state is kept but
`recovery_required` is set, the standard `RecoveryRequired` signal is emitted
when possible, and normal mutation is rejected until a known-good restore —
the restore is never retried as if nothing changed. Regressions:
`restore_emits_the_bounded_restore_commit_event`,
`restore_audit_failure_demands_recovery_and_fences_mutation`.
**CORE-03 closed.**

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
- process-capability cancellation kills a Unix process group or invokes
  Windows `taskkill /T`, and `shell.exec` / the git tools do the same
  through the shared `agent_process::kill_process_tree` — a cancelled
  shell no longer leaves `&` background jobs or `start`-spawned
  descendants running, and a timed-out git cannot leave hook/alias-spawned
  subprocesses alive (the M12 stale-mutation boundary); Windows enforces
  Job-Object quotas (active-process ceiling, per-process memory ceiling,
  `KILL_ON_JOB_CLOSE`); every process path has bounded output (model tail
  + artifact spill, capped stderr tail, capped provider stream);
- approval timeout/cancel now cleans up every pending entry (broker and
  decisions map) and the wait ends the moment the operation is cancelled;
  the UI preview is bounded (220 chars) and only explicit y/n/Enter/Esc
  resolve a prompt — ambiguity or truncation never becomes allow;
- provider streams now carry a total byte cap; EOF/backoff boundaries
  already cancel explicitly.

**2026-08-11: closed.**

1. **Shared tree-kill** — `agent_process::kill_process_tree` (Unix
   process-group SIGKILL, Windows `taskkill /T`) is now the one primitive
   behind the process-capability host, `shell.exec` (group leader on
   Unix; kills the tree on cancel/timeout) and the git tools (explicit
   spawn + wait; tree kill on cancel/timeout). Regression tests:
   `shell_cancellation_kills_descendants` (a descendant heartbeat freezes
   after cancel) and the existing git/process tests. The Windows test
   command carries no nested quotes — Rust's argument escaping would
   mangle `\"` for cmd.exe.
2. **Approval pending cleanup** — `ApprovalGate::authorize` now takes the
   operation's cancel token: a cancelled operation stops waiting
   immediately (no 5-minute stall) and every exit path removes both the
   broker entry and the decisions entry (the old cancel/timeout branches
   leaked the oneshot sender). Tests: `cancelled_approval_cleans_up_
   pending_entries`, `timed_out_approval_cleans_up_pending_entries`,
   `answered_approval_resolves_and_cleans_up`.
3. **Provider stream cap** — `OpenAiConfig::max_stream_bytes`
   (`DEFAULT_MAX_STREAM_BYTES = 16 MiB`) counts every decoded SSE line and
   fails the transport instead of growing the accumulator forever.
   `stream_over_cap_fails_bounded` drives a local mock SSE server that
   streams forever and asserts the tiny cap refuses.
4. **Windows Job-Object quotas** — the process sandbox creates a
   Job-Object when quotas are requested (`JOB_OBJECT_LIMIT_ACTIVE_PROCESS`
   from `process_limit`, `JOB_OBJECT_LIMIT_PROCESS_MEMORY` from the new
   `job_max_memory_bytes`, `KILL_ON_JOB_CLOSE`); the capability adapter
   defaults to a 512 MiB per-process ceiling. `kill_tree` terminates the
   job in one kernel call, falling back to `taskkill /T` when no job
   exists. Assignment degrades (never fails the connection) when an outer
   Job-Object on CI runners blocks nesting. Tests (Windows, skipped under
   an outer job): `job_object_assigns_and_terminates`,
   `job_object_caps_active_processes`.

The standing-grant `TaskExecutionPolicy` (narrow effect + target +
constraint + expiry grants replacing the per-call prompt) remains
`CORE-08`, not a CORE-06 residue.

### CORE-07 — Workspace operations remain TOCTOU-sensitive

Canonicalization blocks known pre-planted escapes, but validation and later
open/create can race a link swap. Trusted Core should use directory-handle-
relative/openat-style operations (and Windows equivalent), reject reparse
substitution at operation time, and test concurrent swaps.

**2026-08-11: closed.** The workspace fuses validation and open into one
directory-handle-relative descent (`crates/agent-workspace/src/confined.rs`
— `ConfinedDir`/`ConfinedFile`):

1. **Directory-handle-relative opens** — reads (`Workspace::confined_open_
   read`) and mutation parents (`confined_parent`) descend through pinned
   directory handles: `openat` with `O_NOFOLLOW`/`O_DIRECTORY` on Unix,
   `NtCreateFile` with a `RootDirectory` handle on Windows. A handle pins
   the directory object, so renaming the path after the handle is held
   cannot redirect the next step.
2. **Reparse substitution rejected at operation time** — every Windows open
   uses `FILE_OPEN_REPARSE_POINT` and then rejects any nonzero reparse tag
   (symlink, junction, mount point, cloud placeholder), so a swap into a
   link fails the open instead of following it.
3. **Handle-relative atomic replace** — the staged temp file is created
   exclusively under the pinned parent and committed with `renameat` /
   `NtSetInformationFile(FileRenameInformation)` relative to it, so the
   final mutation cannot be redirected either. (The
   `SetFileInformationByHandle` wrapper rejects a nonzero `RootDirectory`
   on this Windows generation; the native call is used.) The old
   path-based `atomic_replace` is gone.
4. **Consumers read through the handle** — `WorkspaceHandle::read`,
   `fs.read` and `edit.replace` take metadata and content from the pinned
   handle, so size checks and reads describe the same object. Path-string
   `resolve_relative` remains for display-only resolution (`fs.list`,
   `search.grep`, `git.diff` pathspec validation).

Regression coverage (the audit's "test concurrent swaps"): a real
directory and an outside-pointing junction/symlink are swapped in and out
of the target path by a real OS thread while the victim reads (`concurrent_
dir_swap_never_reads_outside`) and mutates (`concurrent_dir_swap_never_
writes_outside`) — outside content never surfaces and no mutation lands
outside. Also: `confined_read_rejects_preplanted_reparse_link`,
`confined_mutation_creates_missing_parents`,
`confined_replace_file_overwrites_and_creates`.

### CORE-08 — Per-call approval cannot support unattended long tasks

The current policy has two extremes:

- `PolicyApprovalGate` uses global booleans for all WorkspaceWrite and all
  ProcessExecution calls;
- `InteractiveApprovalGate` prompts on every non-read-only call, waits up to
  five minutes, then denies on timeout.

**2026-08-11: closed.** `TaskApprovalGate`
(`crates/agent-kernel/src/approval.rs`) wraps any inner `ApprovalGate` with
task-scoped standing grants established by the composition root
(`agent-tui` parses `--grant=<json>`), revocable via `revoke` and visible
via `/grants`:

1. **Standing-grant structure** - `StandingGrant` (agent-contracts) binds one
   `ToolRisk` effect to a `GrantTarget` (workspace path prefix and/or
   process command prefix) with a `GrantConstraint` (`max_content_bytes`,
   `max_runs`) and `expires_at_ms`. The model can *use* a matching grant
   (no per-call prompt) but can never create, widen or extend one:
   `grant()` is composition-root/UI-only, `ReadOnly` grants and scope-less
   grants are rejected at grant time, and an expired, revoked or exhausted
   grant silently stops matching and falls through to the underlying gate.
2. **Component-aware target matching** - a workspace write matches only when
   its path is at or under the granted prefix at the component level
   (`src/../outside/x`, `../src/x`, absolute paths and drive-qualified paths
   never match), and a process call matches only when its command starts
   with the granted whitespace-token prefix (`cargo testx` does not match
   `cargo test`).
3. **Bounded consumption** - `max_runs` is consumed once per matched process
   call; `max_content_bytes` caps the write content; expiry is checked on
   every decision and in `active_grants`.
4. **Zero-responder semantics** - a granted call resolves without waiting
   (test asserts < 40 ms); an ungranted call falls through to the inner
   gate, so a missing responder can never expand privileges. Residual: an
   ungranted call behind the interactive gate still waits its configurable
   `answer_timeout` (default 5 minutes), so fully avoiding the per-call
   stall needs a shorter inner timeout or deny-by-default; the "aggregate
   unavoidable boundary choices at a checkpoint" direction remains future
   work.

Regression coverage (the audit's "zero responses produces no privilege
expansion and no per-call stall"): `standing_grant_allows_matching_write_
without_prompt`, `write_outside_grant_delegates_to_inner`, `grant_prefix_is_
component_aware`, `parent_and_absolute_writes_never_match_a_grant`,
`expired_grant_stops_matching`, `revoked_grant_stops_matching`,
`process_grant_limits_runs_and_prefix_is_lexical`, `content_cap_rejects_
oversized_write`, `grant_rejects_invalid_targets_and_shapes`,
`zero_responder_without_grant_denies_without_expansion` (agent-kernel).

### CORE-09 — Tool schema budget mutates lifecycle and can forget required capability

Status: **[~] TaskToolRequirements/round-surface slice verified; complete
TaskAnchor policy and per-tool capability lifecycle remain open.**

The verified first slice makes runtime-owned `RoundSurfacePlan` the sole
schema-budget projection over the complete loaded candidate catalog. Each
TaskRecord now owns a bounded, revisioned, whole-set-CAS requirement set with
`MustSurface`, `PreferSurface`, and `KeepReady`. Budget omission is round-local
and never unloads a catalog entry; Must either appears or produces a typed
pre-provider refusal; KeepReady repairs GC eviction but remains prompt-cold.
The final immutable snapshot and bounded, schema-free `ToolSurfacePlanned`
report carry a monotonic surface revision plus non-colliding catalog/task/focus
source revisions. `ModelStarted` is emitted only after successful final
packing. RuntimeCheckpoint v2 persists task requirements and counters, never a
derived round surface.

Regression tests cover non-mutation, deterministic omission and budget
recovery, KeepReady reload, Must refusal with no provider call or
`ModelStarted`, event ordering/bounds, revision monotonicity, and
checkpoint/suspend/restore reconstruction. They also cover atomic builtin
capture, capability surface-gate serialization, and a composite common cut
under concurrent catalog mutation. Full workspace tests and strict Clippy
pass.

This still is not the complete TaskAnchor. Requirements are explicit exact
tool names rather than projections from typed goal/phase/open-loop/acceptance
state. Dynamic capabilities also use one owner-level `loaded: bool`, so
loading one tool can mark sibling schemas loaded and external tools do not yet
receive independent builtin-style idle cooling. Snapshot consistency is closed
for the first slice: builtin specs/generation share one registry lock,
capability mutations and capture share the surface gate, and the composite
dispatcher holds that gate across one atomic base snapshot so both sources
form one common cut without retry. One explainability
gap remains in the first-slice surface itself:

- report rows collapse a task-authored `PreferSurface` requirement and an
  ordinary catalog-loaded optional candidate into the same demand value. A
  selected/omitted row cannot yet answer whether it entered because of Task
  intent, dispatcher/core policy, explicit catalog load, or fallback packing.

Required direction:

- [x] separate catalog/authority, operational lifecycle, and one-round
  surface for the TaskToolRequirements slice;
- [x] token/schema budget performs pure round-local packing and never unloads
  a tool or changes its lifecycle;
- [ ] derive typed tool roots from the complete TaskAnchor/Focus/Episode and
  Active-call policy at the existing BeforeModel safe point;
- [x] `MustSurface` tools are selected or produce explicit unsatisfiable
  reports; `PreferSurface` omissions are observable but do not mutate
  lifecycle; `KeepReady` stays cheap to reactivate without entering prompts;
- [ ] make external capability lifecycle per tool while process start/stop remains
  owner-level; loading one tool must not expose all siblings;
- [~] use one monotonic surface revision with separate catalog,
  task-requirement and focus sources; complete Anchor/Episode/execution-policy
  source revisions later;
- [x] pair builtin specs/generation under one lock, serialize capability
  capture and mutation through the surface gate, and hold that gate while
  taking one atomic base snapshot; concurrency tests verify recorded source
  revisions agree with the captured specs;
- [ ] add bounded per-row provenance (`TaskRequirement` with requirement
  revision/reason ref, `DispatcherRequired`, `CatalogLoadedOptional`, later
  Focus/Active/RecentUse) so Task Prefer is distinguishable from a legacy
  catalog optional in both selected and omitted reports;
- [x] publish a bounded round-surface event/ledger entry with selected/omitted
  names, reasons, schema-token totals, provider input budget and source
  revisions; emit `ModelStarted` only after final packing succeeds;
- [x] checkpoint task requirement authority and counters, not a derived
  per-round surface or `Active` state; complete Anchor/lease authority later.

Acceptance: shrinking then restoring a provider budget produces identical
catalog lifecycle; every rooted required tool appears or the round fails with
an explicit reason; optional omission never bumps catalog generation; one
capability tool does not surface siblings; quarantine after snapshot still
revokes execution; suspend/resume reconstructs the surface from TaskAnchor
without transcript replay. Under concurrent catalog mutation, every snapshot's
specs match its recorded generations, and every surface row explains which
authority/demand source put it into consideration.

## P2 — policy quality and evaluation

- terminal semantic checks should precede pinned retention;
- replace weak `SharesEntities` pseudo-dependencies with typed edges;
- add store corruption/reconcile and lifecycle growth-slope metrics;
- [x] fact comparison replays cost and coverage on independent fresh engines;
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
   and fault tests; do not combine with scoring changes. **Done** — closed
   with `CTX-10` (`b7a1330` → `110fd6c`).
2. **Context properties:** residency × lifecycle tests for CTX-01/02/03
   before policy changes.
3. **Store integrity:** CTX-04/05 plus crash injection/reconcile; no scoring
   edits.
4. **Context/runtime consistency:** CTX-06 operation gate plus CORE-03
   cross-plane capture and live-restore rebase audit transaction.
5. **Materializer:** remaining CTX-07/08 packing, candidate-cost and immediate
   tool-signal work; no store edits.
6. **Trusted effect + sandbox:** CORE-01/06/07/08 in M12 -> M14 order.
7. **Durability:** CORE-02 plus recovery replay, isolated from policy.
8. **Prompt/resource boundary:** CORE-04/05 with adversarial evals.
9. **Tool-surface hardening:** CORE-09 per-row demand provenance and per-tool
   capability lifecycle; do not fold in the complete
   TaskAnchor/CompletionRecord package.

Do not run packages editing the same crate concurrently unless file ownership
is explicitly partitioned.
