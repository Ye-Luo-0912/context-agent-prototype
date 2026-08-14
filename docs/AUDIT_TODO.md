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
- [x] A failed mandatory turn commit now persists `recovery_required`; the
  following user/task mutation is rejected until a known-good restore.
- [x] Full runtime restore is two-stage: actor task/context state remains
  recovery-fenced while capability state is applied; an old Enabled snapshot
  cannot lift live Disabled/Quarantined authority, unknown ids do not count as
  applied, and only durable `RuntimeRestored` releases the fence.
- [x] Process capability responses with an empty effect list decode as the
  current envelope, and current/legacy results have `call_id`/`tool_name`
  overwritten from the trusted request rather than producer output.

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

Residual correctness follow-up — **closed 2026-08-11**: episode rotation now
resets the episode-local turn counter (`close_focus_episode` zeroes
`FocusState.generation`), so the `episode_max_user_turns` guard measures the
current episode only; one overlong episode no longer permanently exhausts
every later episode's budget. The cadence regression
`one_overlong_episode_does_not_exhaust_later_episode_budgets` proves the
guard fires at the budget boundary and that the next episode's related
messages do not rotate again.

Companion bound for the same 10K-turn acceptance: inside a long *open*
episode, related messages share tokens, so the score floor keeps them
Active forever and the focus-scope root accumulates every turn (a 500-turn
episode held ~500 messages once the exhausted-budget rotation stopped
firing every message). GC now ages ordinary dialogue — Working retention,
no promotable outcome (decision/finding/constraint/open-loop/artifact/
evidence), not hot, not model-directed, older than the staleness window
residency uses (`ttl x 4`) — out of the heap into the reversible buffer,
and the reactivate phase refuses to bounce it back on its own score.
`focus_generation` diagnostics now read as episode-local. Regressions:
`open_focus_ordinary_dialogue_ages_out_without_rotation`; the
`long_task_10k_turns` peak stays below 200 without relying on the
exhausted-budget rotation to keep it small.

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

Residual correctness follow-up — **closed 2026-08-11**:
minor TTL/tombstone aging no longer iterates Resident items only. The
maintenance pass now tombstones warm-buffer items on the same windows
residency uses (ephemeral TTL, then `ttl x 4` staleness), with pinned and
keep-alive/lease items exempt exactly like the resident root set; a
tombstoned warm item is never reactivated by hot entities. Regressions:
`warm_ephemeral_item_is_tombstoned_by_ttl_not_only_resident`,
`warm_working_item_is_tombstoned_by_staleness`. Stored metadata now
represents every protection field too: `ExternalizedContext` carries
`keep_alive`/`lease_until_turn` (captured at externalize time, cleared by
task completion in every body location), so a protection survives a
buffer-overflow externalize and a completed task cannot keep rooting its
records through a stored reference. Regressions:
`external_entries_carry_the_model_protection_fields`,
`completed_task_clears_protections_in_external_entries`. The authority/body
split landed 2026-08-14 as `ContextCatalog`: one `item_id -> location`
directory plus shared query indexes. GC moves location; it does not copy
authority metadata onto a second record. Bodies remain in heap / warm /
store (the serde layout). Duplicate ownership is still detected by
counting the three stores, because a catalog rebuild skips a duplicate.
Regressions: `catalog_assigns_exactly_one_location_per_id`,
`stored_search_ids_use_label_and_entity_indexes`. Graded retrieval access
(`CTX-GC-11`) landed 2026-08-14: search/inspect/fetch/ack write through the
stored body (`AccessSignal`); search cannot pin Cold entries. Regressions:
`search_saturation_cannot_pin_cold_entries_across_gc_passes`,
`identical_search_query_budget_blocks_a_second_stamp_in_the_same_turn`.

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

Original confirmed defects (contained below unless listed as residual):

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
- **Verifiable completion body + retained raw response** (`110fd6c`). Each
  `CompletionRecord` carries a deterministic ref
  (`task:<id>:completion`) and SHA-256 digest for its bounded completion
  summary. When an artifact workspace is wired, the complete final assistant
  response is stored separately before ContextItem truncation and attached as
  an artifact ref. A dedicated raw-body digest remains future evidence work.

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
  adapter wraps it as `CapabilityOutcome::Value`, bypassing the Runtime/Core
  epoch fence, cancellation and effect rollback;
- an in-process capability granted `workspace:write` can call
  `WorkspaceHandle::write`, which applies the mutation immediately during
  `invoke`; only the sibling `prepare_write` path reaches a staged Effect;
- a side-effecting process tool can self-declare `ReadOnly`, which approval
  auto-allows;
- cwd + env filtering + Unix rlimits are not OS-level isolation; a hostile
  child can still open absolute paths or sockets directly at the OS layer
  (brokered access is permission-gated; direct OS access is not) and
  Windows has no equivalent of the Unix rlimits;
- `manifest.id` enters a predictable temp path without strict path-safe id
  validation;
- inherited stderr is unbounded.

External process capabilities must remain disabled until this closes; M13
is not yet a completed trust boundary. The old wire staging path is now
fail-closed pending PLAT actual-intent proof.

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

**Partially repaired 2026-08-10; wire mutation staging was disabled after the
2026-08-13 authority audit; mid-invoke system broker landed 2026-08-12.** The
trusted in-process/runtime-owned mutation path is fenced, non-empty process
effects fail closed before staging, mid-invoke filesystem reads and network
requests are brokered and permission-gated (item 9 below), and external
process capabilities remain Disabled by default. The OS-level isolation
defect below keeps CORE-01/M13 open.

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
   capability may declare `workspace:write`, but a non-empty process wire
   effect is currently refused before staging until PLAT actual-intent proof
   exists (item 8 below).
3. **No direct capability mutation** — the runtime hands a
   `workspace:write` capability a `StagedOnlyWorkspace` handle whose
   `write` is refused ("must be staged") and whose `prepare_write` returns
   an `Effect` committed by Core behind its authority-epoch fence
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
   over-granted tool risk, process `workspace:write` can register but its
   non-empty wire effect is refused, read-only process allowed),
   `undeclared_permissions_receive_no_handle` updated (direct write refused,
   trusted in-process staging remains Core-committed,
   unknown permission refused at registration),
   `from_manifest_rejects_ids_that_could_escape_a_path`,
   `from_manifest_rejects_readonly_tools_on_write_capabilities`,
   `private_capability_dirs_are_unpredictable_and_path_safe`, and
   `stderr_is_drained_into_a_bounded_tail` (a 4 MiB stderr flood leaves an
   8 KiB tail ending in the newest bytes).
8. **Wire-level effect contract (temporarily fail-closed)** — `WireEffect`
   still describes the candidate `workspace_write` shape, but the process
   adapter rejects every non-empty effect list before base64 decode, staging
   or workspace mutation. Broad `workspace:write` plus an untrusted path is
   not proof that actual intent is inside the lease. Empty-effect and legacy
   plain `ToolOutput` responses remain usable. Re-enabling this path requires
   PLAT-03/04 to bind `operation_id + effect_id + argument_digest` and typed
   actual intent to Core authority. Tests assert `prepare_write` remains zero
   and files remain unchanged with or without a grant/handle. Composite
   effects remain relevant to trusted in-process builtins: they commit
   sequentially and now truthfully report Applied+DurabilityFailed when an
   earlier member landed before a later failure.
9. **Mid-invoke system broker** — a child can issue `{"system": <op>, ...}`
   frames during an invoke; `ProcessHost` (`agent-process`,
   `call_with_cancel_and_broker`) routes them to a `SystemBroker` the
   adapter installs (`InvokeFsBroker` in `agent-capability-process`) and
   continues the exchange, so the broker is the enforcement point for
   "experimental code cannot exceed the permissions granted to it".
   `fs.read` is served only with the invocation's `workspace:read` grant,
   only through the confined workspace handle, and only for relative,
   non-escaping, non-rooted paths — absolute/rooted paths are refused even
   where the OS does not call them absolute (Windows), so the boundary does
   not depend on the handle's implementation. Network system ops
   (`net.fetch`, `net.connect`, `http.get`, `http.request`) are refused by
   design: the permission vocabulary has no network word, so there is
   nothing to grant. A system frame with no broker installed fails closed
   (connection poisoned, child tree killed); a per-call cap
   (`MAX_SYSTEM_REQUESTS_PER_CALL`) bounds frames so a child cannot flood
   the host; a refused system request is an answer, not a connection
   failure. Tests: `brokered_fs_read_serves_files_inside_the_workspace`,
   `brokered_fs_read_refuses_absolute_and_escaping_paths`,
   `brokered_fs_read_without_the_grant_is_refused`,
   `unknown_system_ops_are_refused`,
   `brokered_network_requests_are_refused_by_default`,
   `network_requests_are_refused_even_with_a_networkish_grant`,
   `a_refused_system_request_does_not_poison_the_connection`,
   `a_system_request_flood_poisons_and_kills_the_connection` (capability
   level) and `system_frames_without_a_broker_poison_and_kill_the_connection`
   (host level).
10. **Response envelope and identity hotfix (2026-08-12)** — an explicit
    `ProcessInvokeResponse` is accepted whether `effects` is empty or not;
    only an actual envelope decode failure enters the legacy path. The adapter
    overwrites producer `call_id` and `tool_name` with the request identity in
    both shapes. Artifact ownership/digest, outbound frame caps and explicit
    feature negotiation remain `PLAT-00/04` work.

Residual (M13; CORE-01 remains open): OS-level filesystem/network isolation
for the child process — a hostile child can still open arbitrary absolute
paths or sockets directly at the OS layer (seccomp-bpf / AppContainer-style
filtering is out of v0 scope) — and Windows Job-Object quota enforcement.
The mid-invoke system broker is implemented for bounded filesystem reads and
deny-by-default network requests. Process mutations remain disabled at the
wire boundary until PLAT actual-intent/operation proof exists; trusted
in-process staged effects still commit behind the Core-owned
authority-epoch/lease fence.

### CORE-10 — Current wire containment landed; common protocol proof remains open

Affected: `agent-process`, `agent-context-service`, MCP and process
capability adapters.

Confirmed defects:

- `ProcessHost` bounded child responses, but request and broker-answer frames
  had no equivalent encoded-size cap, and partial EOF lacked a typed failure;
- the context service read an entire line before applying a limit and sent
  responses without a symmetric bound;
- the MCP client checked size only after `read_until`, did not bound
  notification floods, dropped ownership of its spawned child, and did not
  connect invocation cancellation to process-tree termination/poisoning;
- large broker reads copied a whole value before truncating it at the control
  plane boundary;
- process invocation wire identity does not yet carry the runtime operation,
  attempt, task/scope or deadline needed for recovery and idempotency;
- producer artifact refs are path-confined, current-run bound, and now carry
  owner plus an immutable SHA-256 digest in the sealed locator; live
  captures use an explicit draft form until seal. Producer `call_id` and
  `tool_name` spoofing is closed by the CORE-01 response hotfix above.

Immediate acceptance (`PLAT-00`, before changing transport):

1. one bounded frame codec for both directions, including encoded frame,
   in-flight and cumulative byte/count limits, plus explicit caps on known
   decoded large fields; exact parse-time typed budgets landed in `PLAT-04`;
2. the codec preserves distinct frames independent of OS read chunking;
   malformed/partial/oversize/version/id/envelope faults poison and terminate
   owned sessions, while valid domain errors remain reusable;
3. MCP retains and reaps the child, and cancel/timeout kills the owned process
   tree before late output can be admitted;
4. current broker reads allocate only a bounded prefix and locators are
   run-scoped with owner/digest identity; parse-time JSON DOM budgets
   landed in `PLAT-04`;
5. regressions cover outbound oversize, broker/notification floods,
   same-write multi-frame delivery, stale/pre-sent ids, partial EOF and
   cancel-after-spawn.

**Contained 2026-08-13 (current-wire PLAT-00 slice; common-contract proof landed PLAT-04):**
`agent-process::frame` is the one shared codec — outbound frames are capped
before a byte is written, the in-flight cap is enforced incrementally while
reading (including a single large delivery, which previously bypassed the
bound), and typed `Eof`/`PartialEof`/`Oversize` errors replace the raw reader.
The codec deliberately does not call two frames in one OS read "coalesced":
byte streams have no delivery boundary, so it returns exactly one frame and
preserves the remainder. `ProcessHost` applies the codec in both directions,
uses unpredictable host-owned request ids to reject pre-sent/stale responses,
adds a per-call cumulative byte budget (`max_call_bytes`) and a control-plane answer
cap (`max_system_answer_bytes`, oversized broker answers degrade to a refusal
frame), and any framing violation poisons the connection and kills the child
tree. The context service reads with the same codec, replaces over-cap
responses with a bounded error frame, and ends the session on malformed/
oversize/version-mismatch frames. The MCP client owns its spawned server
child (kill + reap on cancel/timeout/poison, `kill_on_drop`, `Drop`
backstop), validates JSON-RPC id/version/result-vs-error, and poisons on
framing violations plus bounded notification frame/byte floods. It replaces
poisoned clients on the next invoke, tears down via `stop()`, and surfaces `AgentError::Cancelled`
on cancellation. The broker reads through a mandatory allocation-bounded
workspace primitive and serves at most a 256 KiB prefix
(`BROKER_FS_READ_MAX_BYTES`) with `byte_len`/`truncated` metadata. Non-empty
process effects are rejected before base64 decode or staging; decoded-effect
budgets become relevant only if PLAT authority proof re-enables them.
Regressions cover outbound oversize before any byte
is written, cumulative per-call bytes, same-write multi-frame/stale-id,
partial/malformed frames, oversized service requests, notification flood,
broker truncation and MCP
cancel-after-spawn with tree termination and fresh-connection replacement.
**Additional containment 2026-08-13:** process capability responses carrying
non-empty `WireEffect`s now fail closed before staging until a typed actual-
intent proof can be checked against the lease. Composite effects no longer
report `NotApplied` after earlier members landed: they report applied with a
durability/recovery failure and clean every unattempted preparation. Runtime
sets a recovery fence for durability-failed/unknown receipts, refuses further
same-turn tool dispatch, and rejects later commands until restore. Local
shell/process/session readers use bounded byte fragments (4,000-byte channel
items), cap each output artifact at 8 MiB while continuing to drain, and expose
truncation counters. Artifact locators are capped identity strings
(`artifact://v1/<run>/<owner>/<digest>`); paging, `artifact.read`, the output
broker and CompletionRecord admission reject cross-run, path-shaped, draft
(for completion) and digest-mismatched refs.

**Residual:** parse-time decoded JSON DOM budgets, RFC 8785 JCS, explicit
`legacy.invoke-output.v1` negotiation and the shared adapter fault matrix
are landed (`PLAT-04`). Adapter envelope migration onto Platform DTOs remains
`PLAT-07`. General artifact-range transport for very large bodies remains
later work. Current adapters still lack the
operation/attempt/scope/deadline fields defined by the landed `PLAT-02`
envelope. Error paths kill
owned children synchronously, but
uniform "kill then await reap before return" belongs to the supervisor/session
contract in `PLAT-05/06` (MCP already does it explicitly).

`PLAT-02` has now added the pure common envelope/identity/error semantics in
`agent-platform-protocol`, with strict IDs, exact profiles, explicit response
carrier, monotonic deadlines, bounded causality and retry/effect-state
validation. `PLAT-03a1-a4` have also landed the persistent Core-owned
authority epoch: Runtime requests CAS advances and keeps a mirror; Core rejects
stale dispatch/commit; cancellation advances the fence before any await or
cleanup. Core additionally owns a bounded in-memory operation registry that
binds argument/effect identity, prevents duplicate dispatch/commit, preserves
unresolved operations and exposes found/expired-or-possibly-seen/unseen
in-process queries. The bounded seen filter is deliberately fail-closed, so a
collision can reject a fresh ID until journal-backed compaction exists.
Runtime remains the sole orchestrator. PLAT-03a3 now persists epoch and full
operation transitions journal-first behind an exclusive checksummed `sync_all`
barrier and strictly recovers them across restart. Only a structurally
incomplete final fragment is repaired; complete-frame corruption fails closed,
and writes stop before bounded limits could poison the next startup. Unix
creation synchronizes parent directories; Windows retains an explicit
power-loss directory-entry limitation. PLAT-03a4 preallocates a stable Core
`EffectId`, propagates exact operation/digest/effect identity into builtin
workspace mutations, persists strict prepare/commit/rollback evidence, and
reconciles it against current files at startup. Proven states terminalize;
partial, corrupt, unmanaged or ambiguous effects remain unresolved behind a
`RecoveryRequired` mutation fence. Generic shell/process spawn/exit recovery
is landed; out-of-process capability/MCP invoke recovery is landed.
RuntimeCheckpoint v4 now cross-checks a stable durable Core
WAL prefix before restore and never rewinds authority. Typed query/cancel
routes, the authorized transport-independent router, WAL-first acceptance
publication and the RuntimeActor-owned exact-current-tool seam are landed.
WAL compaction is landed (exact-tip ancestors; discarded prefixes fail closed).
In-process authenticated operation-control session installation is landed.
Framed JSON-lines operation-control over an inherited-pipe analogue is landed.
Out-of-process capability/MCP invoke recovery is landed (reserved/dispatch/ack
journal; in-flight keys refuse a second send). HTTP/gRPC brokers are still
absent, so
PLAT-03 remains partial and makes no general crash exactly-once or malicious
same-process Runtime claim. `PLAT-04` common-contract proof is landed
(JCS, legacy negotiation, shared adapter fault matrix). Adapter envelope
migration remains `PLAT-07`. Named
pipes/Unix sockets are not a fix for this defect and
remain a measured, later transport choice.

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
   The runtime also persists `recovery_required = true`; the regressions issue
   another user message and require `AgentError::RecoveryRequired`, proving
   the signal is an enforcement fence rather than an informational event.

Acceptance: a subscriber observes `TurnCompleted` only after a flush barrier
has covered every mandatory state write; a failed barrier surfaces
`TurnCommitFailed`/`RecoveryRequired` and never broadcasts `TurnCompleted`.

Cancellation uses a separate contract: `TurnCancelled` is durably appended
and returned as a typed `TurnCancelAck`, but it never advances the successful
`TurnCompleted` recovery marker. If that cancellation barrier fails, the
operation is already fenced, the caller receives `RecoveryRequired`, and
ordinary mutation stays blocked until restore. This closes the prior bug in
which a cancelled turn could be replayed as a successful commit.

Live `ModelDelta` envelopes do not punch holes in that recovery trace. They
are broadcast-only, repeat the durable cursor of the opening `ModelStarted`,
and never advance the journal sequence; persisted events therefore remain
contiguous from 1 through cancellation and `RunCompleted`. Regressions cover
both a completed streaming turn and streamed cancellation followed by
shutdown.

Residual — **closed 2026-08-11 (crash-recovery replay)**. The recovery
machinery now re-reads the trace to rebuild state after a barrier failure.
`agent-replay` gains a recovery module (`src/recovery.rs`) and a
`--recover <trace.jsonl>` CLI mode that:

1. **Locates the durability barrier** — the last successful `TurnCompleted`
   seq is the committed boundary; the first `TurnCommitFailed` is reported
   with its phase and message (there is at most one: the runtime drops the
   turn frame at the first failure), and the count of events after the
   failure is surfaced (the runtime fences mutation after
   `RecoveryRequired`, so a large tail is itself a red flag).
2. **Checks the envelope sequence** — seq must be contiguous from 1; the
   first gap `(expected, found)` is reported so lost/duplicated events on
   disk cannot masquerade as a complete trace.
3. **Rebuilds the context-engine state from the events** — a fresh engine
   replays the run deterministically (ingest/maintain/materialize/GC/
   consumption), producing the final diagnostics a recovery can trust.
   Scope honesty: the trace is an audit stream, not a state-replay log —
   the context plane is fully reconstructible; `TaskManager` detail
   (anchor content, requirement revisions) is checkpoint-only by design.
4. **Proves restore consistency** — `verify_restore_consistency`: restore a
   context checkpoint into a fresh engine, replay the events after its
   cover seq, and compare every diagnostics dimension with a full rebuild.
   Agreement is the engine-level guarantee that the runtime and the context
   never drift apart after a crash recovery; a wrong cover seq (the caller
   claiming a checkpoint covers fewer events than it captured) is detected
   as a divergence.

Regressions: `recovery_locates_the_last_committed_barrier`,
`recovery_reports_the_turn_commit_failure_and_fences_after`,
`recovery_detects_seq_gaps`, `recovery_rebuilds_context_state_
deterministically`, `restore_then_incremental_replay_matches_full_rebuild`
and `verify_restore_consistency_detects_a_wrong_cover_seq`. A genuinely
full disk remains covered by the sticky-error path but is still not
exercised with a real full volume (the barrier contract and the recovery
machinery are closed; the volume-level exercise stays an ops concern).

## P1 — confirmed defects and hardening

### TOOL-01 — `search.grep` never observes its cancellation token

Confirmed 2026-08-14. `SearchGrepTool::execute` receives the runtime's
`CancellationToken` as `_cancel` and never checks it: the file walk (up to
`MAX_FILES_SCANNED = 5_000` files), the per-file reads (up to 2 MiB each)
and the regex scan all run to completion even after the turn/operation is
cancelled. The scan is bounded, so this is wasted work and cancellation
latency, not an unbounded hang — a cancelled turn must not keep paying
for a dead query. Fix: check the token between
files (and periodically inside large files), return the partial result as
an explicitly cancelled outcome, and add a regression that cancels
mid-scan.

**Closed 2026-08-14.** `search.grep` checks the request token before the
walk, between files, and every 256 lines inside a file; `walk_files`
accepts an optional token so the shared scanner can stop without changing
`code.symbols`. A cancelled scan returns `Ok(ToolOutcome::Value)` with
`ok: false` and `metadata.cancelled`, keeping any hits already found —
`Err(Cancelled)` would be stripped by Core into an empty
`tool_error_output`. Cancelled scans do not write a paging artifact.
Regressions: `grep_honors_preexisting_cancellation` and
`grep_stops_mid_scan_and_returns_partial_hits`.

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

**Cancellation/shutdown cleanup bound closed 2026-08-13.** Cancellation
installs the Core-owned epoch fence before awaiting scope cleanup. Tool-frame
closes now have bounded per-scope and total deadlines; a timeout emits the
error, raises `RecoveryRequired`, and refuses to return a durable cancellation
acknowledgement. A cancelled tool operation remains an explicit pending-cleanup
root, blocking normal mutation. `Stop` consumes its late completion under a
hard deadline and routes any `PreparedEffect` through stale rollback before
Core shutdown; it reports `RecoveryRequired` rather than silently dropping an
unresolved effect. Regressions:
`cancellation_bounds_a_hanging_tool_scope_close_and_fences_mutation` and
`stop_drains_a_cancelled_tool_before_dropping_its_prepared_effect`.

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
Only the public `RuntimeInstance::checkpoint/restore` carries host capability
state; the actor-only checkpoint command is crate-private so callers cannot
persist it as a complete snapshot. A rejected actor restore leaves
activation/load flags untouched.

Live restore is a two-stage instance commit. The actor first installs
context/task authority while remaining recovery-fenced; the registry then
applies a fail-closed activation meet (`Enabled < Disabled < Quarantined`);
only the final durable audit releases the fence. That bounded
`RuntimeEvent::RuntimeRestored` carries checkpoint version, restored and
current run ids, old/restored/effective focus and surface revisions
(`RestoreRevision`), the rebased task-requirement count plus a capped 16-id
sample (artifact spill reserved for full detail), and whether any registered
capability state was actually applied. Unknown snapshot ids report false; an
old Enabled row cannot lift a live Disabled/Quarantined capability or rebuild
its surface. If appending this mandatory barrier fails after the
context + task authority commit, the restored aligned state is kept but
`recovery_required` is set, the standard `RecoveryRequired` signal is emitted
when possible, and normal mutation is rejected until a known-good restore —
the restore is never retried as if nothing changed. Regressions:
`restore_emits_the_bounded_restore_commit_event`,
`restore_audit_failure_demands_recovery_and_fences_mutation`,
`prepared_restore_stays_fenced_and_unpublished_until_finalize`,
`capability_restore_cannot_promote_live_disabled_or_quarantined_authority`,
and `capability_restore_reports_only_registered_rows_as_applied`.
**CORE-03 closed.**

### CORE-04 — Output broker/resource policy is incomplete
The actor hard limit protects model context, but a producer without an
artifact loses the truncated middle. Process responses may allocate to the
frame limit, metadata/summary can be large, and inherited stderr bypasses it.

**2026-08-11: closed.** A trusted output broker runs inside the kernel
before any `ToolOutcome` reaches the actor:

1. **Every field is capped.** `summary` (2 000 chars), `model_content`
   (16 000 chars) and serialized `metadata` (8 000 bytes) each have a hard
   cap, plus a decoded-total cap on the combined model-facing view (24 000
   chars). Oversized fields carry a visible truncation marker naming the
   field, the original size and the artifact reference.
2. **Oversized content spills once, never truncates away.** When
   `model_content` exceeds the cap and the producer did not return an
   artifact, the broker stores the full content under
   `.focus-agent/artifacts/<run>/` and returns a bounded head/marker/tail
   preview with the `artifact://` reference — a producer without an
   artifact no longer loses the truncated middle. A producer's own reference
   is preserved only when it resolves to a readable artifact in the current
   run; forged or cross-run locators are removed and oversized content gets a
   trusted replacement spill.
3. **Applied to context fetch and provider errors.** `context.fetch` items
   pass through the same broker after the engine answers (large stored
   content spills), and provider/model error text is capped before it
   enters the event stream (`bound_error_message`, 4 000 chars). Inherited
   process stderr stays bounded at the process layer (bounded stderr,
   artifact tail), not by this broker.
4. **Query limits are enforced in execution.** `context.search`'s limit is
   clamped to `CONTEXT_SEARCH_MAX_LIMIT` (50) inside
   `resolve_engine_query`, so a hostile or stale limit cannot ask the
   engine for an unbounded hit set even if it never touches the JSON
   schema; `0` still means the engine default.

Wiring: `agent-contracts` owns the `OutputBroker` contract and the caps;
`agent-workspace` provides `WorkspaceOutputBroker` (composition-root
implementation); `agent-core` applies it in `execute_tool` and
`resolve_engine_query` when the config carries one; `agent-tui` injects it.

Regression coverage: `oversized_content_spills_to_an_artifact_and_keeps_
both_ends`, `existing_current_run_reference_is_preserved_not_overwritten`, `cross_run_or_forged_reference_is_replaced_before_truncation`, `summary_and_
metadata_are_capped_independently`, `decoded_total_cap_trims_content_when_
fields_combine_over` (agent-workspace); `output_broker_bounds_tool_results_
before_the_actor`, `context_fetch_results_are_bounded_after_resolve`,
`search_limit_is_clamped_in_execution`, `search_limit_zero_keeps_the_engine_
default` (agent-core); `output_broker_spills_oversized_tool_output_end_to_
end` (agent-runtime actor); plus the provider-error cap tests
(agent-runtime `output.rs`).

### CORE-05 — Untrusted historical content is promoted to System role
`PromptAssembler` renders selected historical user/tool/file content and
external summaries inside `ModelMessage::system`, giving retrieved prompt
injection system precedence.

**2026-08-11: closed.** `PromptAssembler` renders every observation as a
low-authority `user` message, never as `system`:

1. **System holds policy only.** `system_policy` (and the focus frame, the
   runtime's authoritative task state) is the only content rendered with
   the `System` role; it never contains retrieved history.
2. **Observations are delimited, low-authority user messages.** The
   selected working set and external refs render as `ModelMessage::user`
   under their existing explicit headers (`SELECTED WORKING CONTEXT` /
   `EXTERNAL CONTEXT (refs only)`), so instructions injected inside a
   retrieved file, tool result or store item cannot gain system precedence
   over the operator's instructions. File/tool content already arrived as
   `Tool`-role messages in the turn frame, and nothing in the turn frame is
   `System`.
3. **Malicious-content evals.** `retrieved_history_never_renders_as_system`
   and `injected_instructions_cannot_gain_system_precedence` seed the
   working set with "ignore previous instructions ..." text and assert it
   appears only inside a `user` observation; `external_refs_render_as_low_
   authority_observations` covers the external map; `malicious_file_and_
   tool_content_stays_in_the_tool_role` covers hostile `fs.read` output
   (prompt.rs, agent-runtime). `model_input_flattens_five_layers_in_order`
   (agent-contracts) now pins the user-role context frame in the flattened
   message order.

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
(`crates/agent-core/src/approval.rs`) wraps any inner `ApprovalGate` with
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
`zero_responder_without_grant_denies_without_expansion` (agent-core).

### CORE-09 — Tool schema budget mutates lifecycle and can forget required capability
Status: **Closed 2026-08-11 - typed tool-root derivation, per-tool
capability lifecycle and per-row provenance verified; surface sources
complete.**

Runtime-owned `RoundSurfacePlan` is the sole schema-budget projection over the
complete loaded candidate catalog. Each TaskRecord owns a bounded,
revisioned, whole-set-CAS requirement set with `MustSurface`,
`PreferSurface`, and `KeepReady`. Budget omission is round-local and never
unloads a catalog entry; Must either appears or produces a typed
pre-provider refusal; KeepReady repairs GC eviction but remains prompt-cold.
The final immutable snapshot and bounded, schema-free `ToolSurfacePlanned`
report carry a monotonic surface revision plus non-colliding catalog /
task-requirement / anchor / focus / execution-policy source revisions.
`ModelStarted` is emitted only after successful final packing.
RuntimeCheckpoint v4 persists task requirements, anchors and counters, never
a derived round surface.

On top of the explicit exact-name set, a pure typed-root policy derives
roots at the BeforeModel safe point: the task anchor's structured fields map
to explicit tool families (acceptance criteria -> verification, open loops
-> exploration, plan progress -> mutation, working refs -> artifact access),
the focus goal without a task derives exploration, and the active-call
policy pins the executing tool as `MustSurface`. Derivation is deterministic,
de-duplicated, catalog-filtered and bounded; the explicit task-owned set
stays the authority. Dynamic capability lifecycle is per tool: loading one
tool of a capability never surfaces its siblings, while process start/stop
stays owner-level, and checkpoint restore migrates legacy whole-capability
flags to per-tool lists. Every selected/omitted round row carries per-row
provenance (`TaskRequirement` / `DispatcherRequired` / `CatalogLoadedOptional`
/ `Unknown` for legacy rows), so Task Prefer is distinguishable from a
catalog load in both selected and omitted reports.

Regression tests cover non-mutation, deterministic omission and budget
recovery, KeepReady reload, Must refusal with no provider call or
`ModelStarted`, event ordering/bounds, revision monotonicity,
checkpoint/suspend/restore reconstruction, atomic builtin capture,
capability surface-gate serialization, a composite common cut under
concurrent catalog mutation, per-tool capability loading/unloading,
snapshot/restore round-tripping the per-tool surface with legacy migration,
typed family derivation, active-call pinning, and per-row provenance on
selected/omitted rows. Full workspace tests and strict Clippy pass.

Required direction:

- [x] separate catalog/authority, operational lifecycle, and one-round
  surface for the TaskToolRequirements slice;
- [x] token/schema budget performs pure round-local packing and never unloads
  a tool or changes its lifecycle;
- [x] derive typed tool roots from the complete TaskAnchor/Focus/Episode and
  Active-call policy at the existing BeforeModel safe point;
- [x] `MustSurface` tools are selected or produce explicit unsatisfiable
  reports; `PreferSurface` omissions are observable but do not mutate
  lifecycle; `KeepReady` stays cheap to reactivate without entering prompts;
- [x] make external capability lifecycle per tool while process start/stop remains
  owner-level; loading one tool must not expose all siblings;
- [x] use one monotonic surface revision with separate catalog,
  task-requirement, anchor, focus and execution-policy sources (the focus
  scope rotation doubles as the episode boundary, so its revision covers the
  Episode plane);
- [x] pair builtin specs/generation under one lock, serialize capability
  capture and mutation through the surface gate, and hold that gate while
  taking one atomic base snapshot; concurrency tests verify recorded source
  revisions agree with the captured specs;
- [x] add bounded per-row provenance (`TaskRequirement` with requirement
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

### EVAL-01 — M15 live evidence is diagnostic, not yet auditable acceptance

Confirmed 2026-08-14:

- live cells aggregate broadcast events in memory, print to stdout and delete
  their temporary workspaces; there is no per-cell manifest, trace, final diff
  or hidden-verification artifact from which the report can be rebuilt;
- broadcast lag is ignored and missing provider usage is indistinguishable
  from a measured zero, so an aggregate can be silently incomplete;
- timeout, round-cap and runtime errors abort the comparison instead of
  becoming intent-to-treat cell outcomes;
- arm order is fixed append → rolling → dynamic; provider time/load drift is
  not counterbalanced;
- the stated 30×3 / −5 pp gate has no frozen paired estimator, clustering,
  one-sided interval, infrastructure-failure rule or power calculation;
- current hidden checks are mostly static file-content assertions;
  live B now uses a model-backed bounded compactor (CI keeps the
  scripted digest); executable hidden build/tests remain open;
- replay Resident `peak` samples pre-model previews rather than every heap
  mutation; report that scope explicitly and never claim an all-time peak.

Required closure:

1. write one versioned, bounded evidence bundle per intended cell containing
   provenance/config hashes, complete gap-checked events, usage completeness,
   final workspace diff/hash, executable hidden-test evidence and a
   machine-readable summary; generate report tables from these bundles;
   **Partial 2026-08-14 (EVAL-01.1).** Live `--compare-live*` now writes
   `agent-eval.cell.v1` under `target/eval-evidence/<unix-secs>/` (or
   `--evidence-dir`): manifest, gap-checked events.jsonl (`ModelDelta`
   omitted; seq check skips those repeats), usage-incomplete and broadcast-lag
   flags, workspace sha256, verify.json, tool histogram, pair.json.
   Failed cells remain in the pair. `--show-evidence` rebuilds the table.
   **Partial 2026-08-14 (EVAL-01.1b).** `verify.json` is now
   `agent-eval.verify.v1`: named file-content asserts, bounded file bodies,
   and `reverify_from_report` after the workspace is gone. This is not a
   pytest/build hidden test; scoring of the five smoke fixtures is unchanged.
   Not yet: executable hidden build/test commands for the 300-task suite.
2. freeze at least 300 independent heterogeneous coding tasks before the run,
   counterbalance arm order, and record every intended cell including task,
   infrastructure, timeout and censored outcomes; do not invent one-line
   stand-ins;
   **Method guidance (2026-08-14):** treat the frozen suite as its own
   reviewed deliverable — source task shapes from real repository
   histories where practical, require executable hidden verification per
   task, and review heterogeneity (language, size, edit shape, multi-turn
   recall pressure) before freezing;
   **Partial 2026-08-14 (EVAL-01.4e / EVAL-01.3b).** Suite pack frozen at
   509/300. `SUITE_FROZEN=true`. SPEC re-registered with retrieval
   secondaries (no gate n/margin change). **EVAL-01.3c:** exact 300
   acceptance ids locked (`--acceptance`, sha256 `7ff6b5dd…`); the gate
   is that set, not any ≥300 subset of the 509 pack. Do not collect
   300×3 acceptance cells until remaining calibration.
3. pre-register a task-clustered paired binary analysis and power simulation;
   three repeats measure within-task variance and are not independent tasks;
   **Partial 2026-08-14 (EVAL-01.2).** `agent-eval --preregister` /
   `--analyze-evidence` freeze the estimand (task-level C−A), Student-t
   one-sided 95% LCL, ITT failure rule, and a 5000-sim table (961/238/49
   passes at Δ=0/−0.05/−0.10). That table shows the historical 30×3 is
   underpowered for −5 pp under A ⟂ C | task. Live arm order is now
   shuffled per fixture×repeat and recorded in `pair.json`.
   **Partial 2026-08-14 (EVAL-01.3).** Same model, seed and margin; gate n
   is 300×3 (4048/258/0). **EVAL-01.3b** freezes the suite
   (`SUITE_FROZEN=true`, pack 509) and declares retrieval secondaries
   in SPEC; n/repeats/margin unchanged. **EVAL-01.3c** locks exact 300
   acceptance ids and requires `evidence_ids==acceptance_ids`; token
   means use only `cost_eligible` pairs; cost-missing rate is separate;
   power-model φ is task-residual corr, not pooled φ. Before the 300×3
   spend, run one frozen non-acceptance pilot (~30×3) to check the
   simulated variance/clustering assumptions against real cells; amend n
   only by re-registration, never after seeing acceptance cells.
   **Partial 2026-08-14 (EVAL-01.5).** 30-id sample frozen
   (`agent-eval.pilot.v1`, 10/10/10, sha256 `fa8c5308…`). `--pilot-run`
   / `--pilot-calibrate` landed; `decision=pilot` cannot pass the gate.
   File-only live 9×3 collected (81 cells, `crates/agent-eval/evidence/pilot-30`):
   ITT A=C=0.778, diagnostic C−A LCL=−0.146, `n_tasks=9 != 300` ineligible.
   **EVAL-01.5.p1 (2026-08-15).** P0 SWE-bench remaining spend skipped
   (send floor 19904 + 12 rounds cannot host the workload). P1: declared
   send window (default 128k), C/B pack 24k, A grows until send, 48
   rounds shared A/B/C. Do not mix P0/P1 ITT tables. Do not amend n.
4. report intent-to-treat end-to-end tokens/rounds/tools/store/retrieval/
   latency. Live C's extra rounds are a treatment effect to explain, not data
   to discard; both-pass cost may appear only as a secondary diagnostic.
   **EVAL-01.3c:** success stays ITT; token means use only cost-eligible
   A/C pairs; cost-missing rate is reported separately and is not in the
   LCL gate. Usage-incomplete 0-token cells are unknown cost, not cost=0.
   **Decision 2026-08-14:** Search/GC evaluation is folded into M15 —
   retrieval metrics (search recall/latency, found-after-forgotten,
   graded-access distribution) are secondary lifecycle endpoints reported
   from the same cells; no separate later retrieval experiment. Declared
   in SPEC at EVAL-01.3b (no gate change).
5. use executable hidden build/tests and a model-backed bounded compaction B.
   **Partial 2026-08-15 (EVAL-01.5.p1b).** Shared `BoundedCompactor` is
   live for B fold and C `TaskCompleted` distill; scripted digest remains
   CI. Executable hidden build/test commands are still open. Do not
   amend n. Do not mix P0/P1 ITT tables.
   **Partial 2026-08-15 (EVAL-01.5.p1c).** Catalog-wide search/inspect;
   prompt stuffing (cache/optional/how-to) was removed rather than
   replaced with a longer tutorial. Extra C rounds are still a treatment
   effect to re-measure; scoring stays frozen.
   **Decision 2026-08-15.** Smoke `FIXTURES` stay interpreter-free
   file-content asserts. Executable hidden stays on the suite pack
   (overlay + commands; SWE-bench docker opt-in). Do not bind the cheap
   CI path to `python`/`cargo` or exec model-written files. Do not amend n.

Until closure, M15, V2, learned/vector policy and PLAT-08 evidence gates stay
closed. M12/M13 remain independent trusted-execution blockers.

- terminal semantic checks should precede pinned retention;
- replace weak `SharesEntities` pseudo-dependencies with typed edges;
- add store corruption/reconcile and lifecycle growth-slope metrics;
- [x] fact comparison replays cost and coverage on independent fresh engines;
- [~] replace the scripted rolling summarizer with a model-backed bounded
  compactor and account for actor, compactor, recall, store, tool-schema and
  wall-time cost;
  **Partial 2026-08-15.** Live B/C inject `ModelBackedCompactor`; CI
  rolling keeps `ScriptedCompactor`. Compactor tokens are on diagnostics
  / `manager_token_cost` / rendered metrics. Actor/recall/store/schema
  wall-time accounting was already on the event stream.
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

Closed CTX/CORE repair packages remain documented above; do not reopen them
as new work. The independent queues that are still actionable are:

1. **Context target:** fix the measured `long_refactor` current-file miss
   without disabling turn-boundary GC or regrowing Resident bytes, then pursue
   TaskAnchorView/root projection, sourced EpisodeOutcome, bounded incremental GC and M15 lifecycle/cost
   measurements. The `ContextCatalog` directory and search indexes landed
   2026-08-14; graded access signals (`CTX-GC-11`) landed 2026-08-14.
   Context + Tool discovery (`CTX-DISC-01..03`, `TOOLS-10`) landed
   2026-08-14 as an internal planner behind `context.manage` /
   `capability.manage` (no public `runtime.search`). Retrieval metrics
   (search recall/latency, found-after-forgotten, access-stamp
   distribution) landed 2026-08-14 on the event stream plus
   `agent-eval --retrieval`. Live paired coding harness
   (`--compare-live` / `--compare-live-all`) landed 2026-08-14; the
   300×3 non-inferiority gate (EVAL-01.3) is still open, and its
   retrieval secondaries ride the same cells (no separate Search/GC
   experiment). Typed user-input envelope (`CTX-EVENT-01..03`) landed 2026-08-14:
   bounded `UserMessageAccepted` preview, optional `user-input` artifact,
   1-slot in-memory queue, `InterruptCommitted` after `TurnCancelled`,
   `Consumed`/`Archived` on successful turns, and `body_ref` replay when a
   workspace is supplied. Dialogue `proposal` is still `None`. Keep
   policy changes separate from store ownership/fault work.
2. **Trusted execution:** CORE-01 plus the M12/M13 generic-process admission,
   recovery and OS confinement residuals.
3. **Protocol/recovery:** CORE-10/PLAT-00 containment, the PLAT-01 narrow
   CorePort/dependency boundary and the pure PLAT-02 semantic contract are
   done, and PLAT-03a1-a4's Core-owned epoch, bounded operation identity/state,
   stale/duplicate-work fence, authority journal and builtin workspace
   reconciliation and checkpoint-v4 authority cross-check are landed. Typed
   query/cancel DTOs, authorized transport-independent router, WAL-first
   acceptance publisher and actor seam are also landed. WAL compaction is
   landed. Out-of-process capability/MCP invoke recovery is landed; a future
   HTTP broker must reuse the reserved/dispatch/ack barrier. PLAT-04
   common-contract proof is landed; adapter envelope migration is PLAT-07.
   Do not confuse workspace-local recovery
   with general crash exactly-once or a non-bypassable same-process
   security boundary, and do not combine semantic recovery work with a
   transport swap.
4. **Real evaluation:** close EVAL-01 first, then run paired, repeated coding
   workloads with executable hidden tests, complete token/tool/store/manager/
   latency cost and the predeclared non-inferiority gate (M15).

Do not run packages editing the same crate concurrently unless file ownership
is explicitly partitioned.
