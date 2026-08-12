# Continuous Context Runtime TODO

Status: code-grounded discussion draft, reorganized 2026-08-10

This document records the target design and implementation order for the
context runtime. It is intentionally narrower than the project roadmap:

- `CONTEXT_LIFECYCLE.md` describes current lifecycle semantics;
- `AUDIT_TODO.md` is the source of truth for confirmed defects (`CTX-*`);
- `TOOL_ECOSYSTEM_TODO.md` owns the modular tool/skill/hook/plugin boundary
  and the coding ACI evaluation plan;
- `ROADMAP.md` owns cross-project milestone order;
- this document connects the long-task product goal to the current code and
  turns it into an integrated context/GC work queue.

An unchecked item is a proposal, not an approved contract. Detailed defect
descriptions remain in `AUDIT_TODO.md` and are referenced here instead of
being duplicated.

Status markers: `[x]` means committed/verified baseline, `[~]` means partial
or uncommitted work under validation, and `[ ]` means open.

## Product and research target

The goal is not to remember an arbitrarily long conversation. The goal is:

> Preserve the long-running task goal and the still-relevant state needed to
> finish it efficiently, keep the model concentrated on its current focus,
> continuously recycle completed execution detail, and retain old evidence
> outside the online working set for deliberate recovery, audit, and replay.

The model request is a disposable view compiled by the runtime:

```text
runtime events + canonical context records + durable evidence
                    |
                    v
       continuous lifecycle/GC maintenance
                    |
                    v
        bounded materialized working set
                    |
                    v
       role-safe prompt for one inference
```

The primary scaling invariant is:

```text
resident items and materialization candidates
  = O(current episode + unresolved live state + explicit protections)
  != O(total task turns or total stored evidence)
```

Success means a long task does not lose its goal, constraints, acceptance
criteria, unresolved blockers, or verified decisions while completed raw
dialogue/tool detail stops consuming online attention and hot-path work.

## Non-negotiable design rules

- The transcript is evidence, not the authoritative next prompt.
- Token pressure is final packing only. It never changes semantic truth and
  never initiates forgetting.
- Context maintenance runs continuously on runtime events, before pressure
  becomes an emergency.
- Context GC manages attention and body residency. Storage GC is a separate,
  conservative retention/deletion policy.
- A summary is a navigation aid, never a replacement for or authority over
  its source evidence.
- Terminal semantic states (`Superseded`, `VerifiedFixed`, `Tombstoned`) never
  resurrect through recall or a derived summary.
- Tool and retrieved content remain lower-authority evidence. Natural-language
  text cannot promote itself into runtime policy or a System instruction.
- v0 remains non-vector. Optional vector/RAG recall is deferred behind the
  replaceable `ContextEngine` boundary until the working-set baseline is
  measured.
- Every selection, promotion, demotion, eviction, externalization, recall,
  and terminal transition must have an observable reason.

## Code baseline at this head

The code is already an event-driven context runtime. It is not a
token-threshold compactor. The next work should improve its semantics,
boundedness, and integrity rather than replace it with transcript history.

| Area | Current state | Code-grounded assessment |
| --- | --- | --- |
| Runtime triggers | Implemented | `ContextMaintenanceTrigger` covers `UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`, `TaskCompleted`, and `Checkpoint` in `crates/agent-contracts/src/context.rs`. `RuntimeActor` invokes these paths in `crates/agent-runtime/src/actor.rs`. |
| Continuous collection | Implemented baseline | `maintain()` runs lifecycle maintenance at runtime events, and `finalize_turn()` runs full `context_gc()` after every committed model turn. `context.collect` can request an additional pass. There is no token-limit trigger. |
| Orthogonal lifecycle axes | Implemented | `ContextItem` separates attention, semantic state, physical residency, retention, and GC generation. This is the right base model. |
| Reversible residency | Implemented baseline | `Resident -> Warm -> Cold -> External` moves old bodies through heap, bounded eviction buffer, and filesystem store; full GC reports eviction/reactivation reasons. |
| Scope ownership | Implemented | Session, Task, Focus, and Tool scopes exist. Runtime owns Task/Tool transitions; the engine owns policy within the contract. |
| Episode-bounded working set | Implemented baseline (`CTX-01`) | A Focus scope now acts as the current episode container. Low lexical overlap or a 500-user-turn guard rotates it; durable/core-labeled results promote and ordinary dialogue becomes evictable. A 10,000-turn test keeps Resident roughly flat. The focus generation is not reset on rotation, so the hard guard currently degenerates into rotation on every later user message after turn 500. |
| Cross-residency lifecycle behavior | Partially fixed (`CTX-02`) | Terminal supersession/verification/recurrence and several completion/protection rules now reach more than Resident. Canonical metadata is still split by location; directives/protections are not fully representable on Stored records and TTL processing still primarily visits Resident. |
| External search/inspect/fetch | Implemented baseline | Exact non-vector lookup and full-body fetch exist, including process-service parity. Terminal external entries are filtered. |
| Transient retrieval result | Implemented baseline (`CTX-03`) | `ToolResultDisposition` keeps search/inspect/fetch transient in the current `TurnFrame`; runtime E2E and context-service parity verify that retrieval does not create duplicate observations. |
| Admission/derivation | Partial | `admit` and `derive` operations, quotas and focused identity/non-duplication tests exist, but broader cross-residency rollback, authority/taint, storage-root and canonical-catalog semantics remain open. Do not infer their full lifecycle contract from the closed transient-retrieval defect. |
| Task anchor | Implemented baseline (`CTX-10`) | Each `TaskRecord` owns a bounded, versioned `TaskAnchor` (goal interpretation, constraints, acceptance criteria, plan progress, open loops, typed root claims) with whole-set CAS, a bounded `TaskAnchorChanged` audit event, and RuntimeCheckpoint v3 persistence + restore validation. The tool-demand slice (`TaskToolRequirementSet`) remains its own bounded CAS surface. A prompt `TaskAnchorView` and Anchor-derived context GC root set (translating claims into engine roots) remain context-runtime work, not reasons to call the anchor itself absent. |
| Structured episode outcome | Partial | Task completion now commits an immutable typed `CompletionRecord` (task id, anchor revision, summary, final-output ref/digest, artifacts) atomically with the status flip (`CTX-10`). Episode rotation still does not derive a typed, sourced `EpisodeOutcome` per rotated focus episode. |
| Task completion output | Implemented baseline (`CTX-10`) | `/done` commits a typed `CompletionRecord` owned by the completed task; `TaskCompleted` events carry task/result identity; the final output body's SHA-256 digest and ref are retained so the outcome is byte-for-byte verifiable after overflow/restart/Storage GC; completed-task records are storage roots, not residency roots (resident heap stays bounded across 1,000 completions). The exact raw final assistant response is still truncated before ContextItem — true raw evidence retention remains the `Immutable raw evidence` row. |
| Canonical catalog | Missing structural target | `context-simple::State` still stores authoritative metadata with the full item in `items`, `eviction_buffer`, or `external`. `CTX-02` repaired behavior but not single-record ownership. |
| Immutable raw evidence | Missing | Ingress truncates the context body to the configured 16,000-character item cap before storage. The current filesystem store preserves that bounded `ContextItem`, not necessarily the original raw output; true raw evidence must be stored once as an artifact/body before context truncation. |
| Store integrity | Implemented crash-recovery baseline (`CTX-04`) | Atomic write/rename, checksums, bounded I/O, post-commit recall deletion, startup reconcile, quarantine, and process-service parity are implemented. Canonical record ownership and the documented quarantine/operator workflow remain broader structural work. |
| Strong provenance graph | Implemented storage-safety baseline (`CTX-05`) | `DerivedFrom`, `EvidenceFor`, `VerifiedBy`, `ArtifactOf`, and `Continuation` are strong edges; `SharesEntities` remains weak. Storage GC roots non-deletable stored records and traverses only strong edges, with deterministic and random-graph tests. Provenance admission/authority policy remains incomplete. |
| Incremental GC work | Missing | Event-triggered minor maintenance scans the entire resident heap; full GC can scan history and issue broad store I/O. The policy is continuous, but the work itself is not yet bounded/incremental. |
| Immediate tool signal | Missing | `AfterTool` persistence/maintenance occurs during turn finalization. A discovery cannot reliably heat related context for the immediately following model round without a bounded metadata-only signal (`CTX-08`). |
| Final model-consumption acknowledgement | Implemented baseline (`CTX-07`) | `materialize` now returns a non-consuming monotonic preview. After PromptAssembler/final packing, a successful non-stale ModelOutput commits a bounded `ContextConsumptionAck` naming turn/operation/round/preview plus exact inline/external ids. Failed/refused/cancelled/stale operations send none; kernel checkpoint rollback couples reinforcement to the `ContextConsumed` audit event. Tests cover actor trim, refusal, cancellation/stale output, external refs, invalid atomic retry, journal rollback, replay and process parity. Candidate cost and external-ref token accounting remain open. |
| Prompt authority separation | Missing | Selected historical/user/tool/file content is currently rendered in System messages. This is tracked by `CORE-05`. |
| Real evaluation | Partial | Unit/property coverage is strong and the 10,000-turn residency regression exists. End-to-end A/B/C task quality, candidate/materialization cost, total manager cost, and failure-recovery metrics are still incomplete. |

This table is the baseline for the work queue below. A checked defect in
`AUDIT_TODO.md` must not be reopened here under a new name.

### Confirmed baseline defects found during this re-grounding

These are correctness work, not policy experiments:

- Episode rotation closes/reopens the Focus scope but does not reset the
  shared `FocusState.generation`. Once the 500-turn guard fires, each later
  user message can rotate again.
- Minor TTL/tombstone processing iterates Resident records. A live item that
  moves to Warm/Cold/External can escape the same aging path and remain
  ineligible for conservative Storage GC.
- Full GC can return early when Resident and Warm are empty even if the
  external map is non-empty, so `gc_epoch`, Cold aging, and automatic recall
  may stop in an external-only state.
- Online dependency marking walks Resident bodies; a marked dependency in
  Warm/Cold is not guaranteed to reactivate.
- Scope-close promotion/attention changes primarily visit Resident bodies;
  old/checkpointed non-resident members can retain location-dependent state.
- Tool-scope close currently discards returned transitions and errors, and
  some `BeforeModel`/manual-collect audit emissions ignore journal errors.
- A store I/O task `JoinError` can lose the externalizing item; successful
  Cold recall can leave an unowned blob; plan/IO/commit has no revision fence.
- Storage GC exists as an engine/service operation but the runtime does not
  schedule it at a task, checkpoint, or retention boundary. Do not add an
  automatic schedule until ownership and strong-edge safety are fixed.
- `ContextDiagnostics.total_items` and `inspect()` currently mean Resident
  items, not total logical catalog items. Existing replay “final total” and
  the transient-fetch test must not be described as catalog-count proofs.
- Replay fact comparison reuses one engine for an observing run and a second
  coverage run, allowing the first run's state to contaminate the metric.

These are assigned to the matching `CTX-*` areas in `AUDIT_TODO.md`; closure
evidence should remain there rather than being duplicated in this document.

## Target model: navigation plane plus evidence plane

The original “managed text collection + pointer + summary” idea should be
implemented as two planes, not as “summary plus a backup transcript.”

### Navigation plane

Small, mutable or rebuildable records used for selection and lifecycle:

```text
ContextRecord {
  id,
  kind,
  task_id,
  scope_id,                  # current Focus scope is the v0 episode id
  attention,
  semantic,
  retention,
  body_location,
  typed_labels,
  typed_edges,
  entities,
  authority_and_taint,
  clocks,
  protections,
  descriptor,               # bounded, derived navigation text
  body_ref,                  # stable logical reference
  revision
}
```

`body_location` changes when GC moves a body. Identity, semantic truth,
retention, protections, labels, and graph edges remain on the same canonical
record.

### Evidence plane

Immutable source bodies and artifacts used for exact recovery, audit, replay,
and re-derivation:

```text
EvidenceBody {
  body_ref,
  item_id,
  content_digest,
  source/provenance,
  immutable raw body or artifact reference
}
```

`body_ref` is a durable logical handle, never a memory address or unstable
path. The body may move between in-memory and filesystem storage without
changing item identity.

### Derived notes are records, not replacements

A summary, task anchor, episode outcome, diagnosis, or workflow is another
record with explicit provenance:

```text
DerivedCard {
  id,
  card_type,
  bounded content,
  derived_from: [item id + optional exact span/turn],
  derivation version/model/prompt,
  source authority and freshness,
  verification status
}
```

It never overwrites its sources. If a summary omits a later-important fact,
the stable references still allow exact recovery. A derived card also cannot
turn terminal source content back into a live fact.

## Task, Episode, and Focus contract

These concepts need distinct lifetimes:

```text
Task
  long-lived goal and completion contract
  |
  +-- TaskAnchor (runtime-owned, small, versioned; never a heap item)
  |
  +-- closed Episode/Focus scopes -> EpisodeOutcome cards + evidence refs
  |
  `-- current Episode/Focus scope
        mutable objective, blockers, active entities, next actions
        `-- short-lived Tool scopes
```

### TaskAnchor

The TaskAnchor is the minimum authoritative state that prevents goal drift
and defines the active task's GC roots. It must live with the actor-owned
`TaskManager`, not as an ordinary Pinned/Durable `ContextItem`: otherwise a
replaceable context policy could collect or rewrite task authority, and task
state would again be duplicated across orchestrators.

#### Current first slice: TaskToolRequirements, not TaskAnchor

The current working tree deliberately lands only the smallest task-owned
subset needed to stop tool-surface decisions from drifting away from the
active task. This subset is named `TaskToolRequirementSet`; it is not a
renamed or partial claim that the complete `TaskAnchor` contract exists.

Status at this stage:

| Status | Scope |
| --- | --- |
| Implemented in the working tree | Each `TaskRecord` owns an exact-name, canonical `TaskToolRequirementSet { revision, entries }`. Whole-set compare-and-swap rejects stale revisions and completed tasks; equivalent normalized replacements are idempotent. The set is capped at 32 entries, with bounded names and reasons. `RuntimeCommand`/`RuntimeHandle` expose the actor-serialized replacement API, `TaskInfo` exposes revision/count, and live restore rebases against a per-process high-water mark to prevent CAS ABA. |
| Implemented in the working tree | `RuntimeCheckpoint` is version 2 and stores every task's requirement set plus runtime focus/surface revision counters. Version 1 can still deserialize far enough to produce an explicit unsupported-version error; there is no silent migration. A derived per-round surface is not checkpoint authority. |
| Implemented and verified | `RoundSurfacePlan` is the sole schema-budget projection over the complete loaded catalog plus the active task's `MustSurface`, `PreferSurface`, and `KeepReady` demands. Actor tests prove GC-triggered KeepReady reload without prompt visibility, pre-provider Must refusal, deterministic provider-budget degradation/recovery without lifecycle mutation, bounded event ordering before `ModelStarted`, monotonic surface revisions, and checkpoint/suspend/restore reconstruction. Full workspace tests and strict Clippy pass. |
| Verified behavior with an audit residual | Live restore rebases focus/surface/task-requirement revisions against per-process high-water marks, preventing revision reuse and CAS ABA. It still lacks a bounded typed `RuntimeRestored`/`TaskRequirementsRebased` commit event; failure to persist that event after state becomes visible must fence the runtime as recovery-required rather than return an unaudited success. |
| Still future TaskAnchor work | Goal/constraint/acceptance authority, plan progress, open loops, evidence and working refs, provenance, autonomous typed patches, `TaskAnchorView`, context materialization roots, and Active/Suspended/Completing/Closed root transfer. |
| Still future Episode/completion work | Typed `EpisodeOutcome`, ack-driven scope/root release for dialogue/reasoning/tool evidence, exact final-response retention, `CompletionPrepared -> CompletionCommitted`, and `CompletionRecord`. Exact model-frame consumption is implemented; the remaining issue is translating verified consumption/outcomes into typed promotion and root-transfer semantics rather than relying on the next-model-round tool-scope close heuristic. |

Demand semantics for this first slice are intentionally narrow:

- `MustSurface`: the exact schema is mandatory for the round or the runtime
  reports an unsatisfiable surface and does not call the provider;
- `PreferSurface`: competes deterministically for schema/provider budget and
  may be omitted without changing lifecycle because of budget pressure;
- `KeepReady`: keeps the exact tool catalog/schema-ready but normally outside
  the prompt. In the current lifecycle this may mean reloading its cheap
  catalog flag after tool GC; it does not mean a capability process is warm,
  does not start a lazy process, and never grants activation or effect
  permission.

Loading remains a lifecycle action rather than an authority grant. The current
dynamic capability registry still has an owner-level `loaded` flag, so loading
one required tool may mark its sibling schemas loaded. Per-tool capability
lifecycle and process/schema separation remain explicit follow-up work; this
slice must not be described as closing that gap.

Target complete shape:

```text
TaskAnchor {
  task_id,
  revision,
  lifecycle: Active | Suspended | Completing | Closed,
  original_goal,
  current_interpretation,
  constraints,
  acceptance_criteria,
  plan_progress,
  execution_profile,
  capability_requirements, # bounded needs; never permission grants
  execution_policy_ref,    # trusted Core policy revision
  current_episode_scope_id,
  open_loops,
  working_refs,             # online residency roots while active
  evidence_refs,            # retention roots; not automatically prompt roots
  last_committed_response_ref,
  completion_ref,
  provenance
}
```

The GC-facing part is typed, not inferred from anchor prose:

```text
ContextRootClaim {
  item_id,
  role: ConstraintSource
      | AcceptanceEvidence
      | ActiveDecision
      | OpenLoopEvidence
      | WorkingArtifact
      | Verification,
  strength: PromptRequired
          | ResidentRequired
          | StorageRequired
          | Recallable,
  source_field_id,
  anchor_revision
}
```

Its semantic content includes:

- original task goal and current interpretation;
- hard constraints and user decisions;
- acceptance/completion criteria;
- plan progress and current episode/focus;
- unresolved open loops/blockers;
- stable references to verified decisions, findings, and artifacts;
- revision and source provenance.

The Anchor may state which execution capabilities the plan currently needs,
but does not authorize them. Each bounded requirement is typed, sourced,
marked Required/Preferred, and scoped to Task/Episode/Operation. Runtime tool
scheduling resolves it against the admitted catalog; the trusted
`TaskExecutionPolicy` decides whether invocation is permitted. An Anchor
patch can therefore ask for a test runner or browser without enabling a
disabled extension or granting network/workspace authority.

It must remain small. Raw dialogue and raw tool output never enter it. An
update is a typed patch with provenance, not a free-form rewrite of the whole
anchor. Each field/list has a hard size/count cap; overflow moves to a
referenced card rather than forcing GC to truncate task authority.

Ownership and authority are not per-step confirmation rules. The user supplies
intent and boundaries; the agent/runtime must maintain the task autonomously:

- the initial request establishes user-authority goal/constraints; the agent
  may immediately derive a bounded current interpretation and testable
  acceptance criteria without asking the user to approve each one;
- the model/runtime automatically maintains plan progress, episode/focus,
  next actions, open loops, evidence refs and inferred criteria through typed
  patches;
- the model may refine an interpretation or add conservative constraints, but
  cannot silently weaken/erase explicit user-authority fields;
- tool/evaluator results may update verification state through typed evidence
  refs, not natural-language claims;
- every patch carries `base_revision`, source authority, and evidence ids;
- patch history lives in the journal/evidence plane; the anchor contains only
  the current reduced state.

User interruption is exceptional. It is justified only when continuing would
materially change the requested goal/scope, waive an explicit hard criterion,
choose between genuinely incompatible high-impact interpretations, or require
an external/irreversible effect outside the standing task policy. Ordinary
planning, coding, testing, GC, evidence promotion and evidence-based completion
must not require confirmation.

No response is never permission. In unattended mode, an unavailable operation
is denied/skipped, the agent continues independent safe work, and the task
finishes `Partial`/`Blocked` with one consolidated boundary report if the
operation was essential.

The actor supplies a bounded `TaskAnchorView` and its typed root claims through
a versioned `ContextTaskView` on materialization/GC requests. The engine
compiles the view but does not own or score it. This preserves the invariant
that model input is built through `ContextEngine::materialize` without making
the engine a second task manager.

Task-state semantics:

- **Active:** the anchor is a mandatory materialization tier; current
  `working_refs` are online residency roots.
- **Suspended:** the anchor remains in RuntimeCheckpoint but is absent from an
  unrelated task's prompt; its refs are storage-retention roots and their
  bodies may cool/externalize. A bounded sourced `ResumePoint` captures the
  current objective, next actions, blockers, active files and evidence refs;
  resume rematerializes only Anchor + ResumePoint refs, not the old transcript.
- **Completing:** freeze the anchor revision and retain Anchor/Focus/output/
  verification roots until a completion transaction commits or rolls back.
- **Closed:** the active anchor is frozen and points to one committed
  CompletionRecord; it is no longer an online root for unrelated work.

### Long-task interaction and permission policy

TaskAnchor authority and effect permission are separate. A TaskAnchor may say
“run the test suite,” but only the trusted Core decides whether a concrete
process/filesystem/network effect is permitted.

The current approval implementation has two unsuitable extremes for long
tasks: global booleans allow/deny every workspace write or process, while the
interactive gate asks once for every non-read-only call and can wait five
minutes. Add a bounded standing policy instead:

```text
TaskExecutionPolicy {
  task_id,
  interaction_mode: AutonomousWithinPolicy | AskAtBoundary,
  grants: [{effect, target_scope, constraints, expiry}],
  explicit_denials,
  no_responder: DenyAndContinue,
  max_interruptions,
  revision
}
```

Typical coding default:

- automatically read/search and perform reversible writes inside the named
  workspace/branch;
- automatically run bounded local builds/tests in the sandbox;
- deny or require an existing narrow grant for external publish/push/deploy,
  broad deletion, secret access, paid/network effects, or communication with
  third parties;
- batch unavoidable boundary choices into one checkpoint request rather than
  interrupting on each tool call;
- if the user does not want to answer, avoid the effect and continue as far as
  possible instead of hanging the task or interpreting silence as approval.

Risk must ultimately be derived from the prepared effect, target, scope and
reversibility—not only the tool's coarse self-declared class. Standing grants
belong to the trusted approval/effect Core and are referenced by TaskAnchor;
the model cannot widen them through a context patch.

### Episode

An Episode is one verifiable subgoal or execution phase. It is neither every
message nor the whole Task.

For v0, reuse the Focus `ScopeId` as the episode identity. Do not add a second
parallel scope tree merely to name episodes. Add an explicit Episode record
only for metadata that a Scope cannot represent.

Episode start signals, in priority order:

1. Task begins or resumes without an open Focus scope.
2. Runtime/plan explicitly activates a new subgoal or phase.
3. The previous episode closes as verified, failed, abandoned, or blocked.
4. A deterministic work guard rotates an overlong episode.
5. Lexical/entity discontinuity remains a bounded fallback proposal, not the
   final authority for semantic correctness.

Short messages such as “continue,” a retry, clarification, and tool rounds
remain in the current episode.

Episode close must be one observable transaction:

1. Freeze the closing scope revision.
2. Construct a sourced `EpisodeOutcome` containing objective, status,
   decisions, findings, artifacts, verification, failures, and unresolved
   handoff state.
3. Validate the outcome and persist its references.
4. Patch TaskAnchor progress/open loops.
5. Promote only cross-episode state: Goal, Constraint, Decision, Finding,
   OpenLoop, ArtifactRef, EvidenceRef, and explicitly durable records.
6. Release Focus/Tool roots and enqueue ordinary dialogue/execution detail
   for collection.
7. Open the next Focus scope if the Task remains active.

If outcome derivation or persistence fails, keep the old scope recoverable
and mark `ClosePending`; never discard the only evidence first.

### Focus

Focus is the small mutable projection of the current episode:

- current objective and next actions;
- active files/symbols/entities;
- latest relevant evidence and errors;
- active decisions/constraints;
- blockers and verification status.

Focus revisions happen after meaningful observations and plan transitions.
Focus is not a transcript and does not have to wait for episode close to shed
consumed detail.

### EpisodeOutcome / work trajectory

```text
EpisodeOutcome {
  episode_scope_id,
  task_id,
  objective,
  status: Succeeded | Failed | Blocked | Abandoned,
  decisions,
  actions_and_results,
  verification,
  artifacts,
  failures_and_root_causes,
  unresolved_items,
  evidence_refs,
  environment/version anchors,
  next_handoff
}
```

The long-term value is the normalized, sourced trajectory—not automatic
re-injection of ancient raw turns. Later workflow/skill extraction must only
promote verified outcomes and remain advisory, versioned, and reversible.

### Task completion and final output

The current code has the importance relation backwards:

- the real final model output is ingested as an ordinary Working
  `AssistantMessage`, truncated by the context item cap, and archived when
  the task closes;
- `/done <summary>` accepts unrelated free text and stores it as a Session
  Durable Summary;
- focus is cleared before that Summary is built, so it loses `task_id`;
- Session Durable records become global GC roots/candidates, so accumulated
  completion summaries can pollute unrelated future tasks;
- neither `TaskRecord`, RuntimeCheckpoint, nor `TaskCompleted` event binds a
  completed task to its final response, artifacts, criteria, or verification.

Replace the free-form completion summary with two linked objects:

```text
FinalOutputBody {
  body_ref,
  digest,
  exact user-facing response,   # persisted before ContextItem truncation
  final_turn_id / event_seq,
  provenance
}

CompletionRecord {
  completion_id,
  task_id,
  task_anchor_revision,
  outcome: Succeeded | Partial | Failed | Abandoned | Cancelled,
  final_output_ref,
  bounded_outcome_card,
  acceptance_results: [{criterion_id, status, evidence_refs}],
  artifact_and_effect_refs,
  verification_refs,
  unresolved_open_loops,
  episode_outcome_refs,
  committed_at,
  schema_version
}
```

Task lifecycle (`Closed`) and outcome (`Succeeded`, `Partial`, etc.) are
separate. Closing a task must not falsely claim success.

Completion is an atomic root transfer:

```text
Active TaskAnchor + current Episode + evidence roots
                      |
              CompletionPrepared
                      |
   persist exact output + validate acceptance evidence
                      |
              CompletionCommitted
                      |
                      v
CompletionRecord + final-output/artifact/verification retention roots
```

Required order:

1. Finish and durably identify the final committed model turn.
2. Persist the exact final output once, before context truncation, with digest.
3. Freeze the TaskAnchor revision and write `CompletionPrepared`.
4. Validate acceptance criteria and build the bounded CompletionRecord.
5. Commit CompletionRecord + terminal anchor patch durably.
6. Only then close Episode/Task scopes and release active working roots.
7. Run context GC; keep the outcome/output/evidence protected from Storage GC
   through strong typed edges.

Any failure leaves `CompletionPending`/`ClosePending`, keeps the old roots, and
is idempotently retryable. A queued journal write without the chosen durable
barrier is not sufficient reason to release the only output/evidence roots.

After commit, the CompletionRecord may cool to Archived/Cold and should not
enter unrelated prompts automatically. It remains directly discoverable by
task id and protects the exact final output, deliverables, and verification
from permanent deletion. A follow-up explicitly fetches/adopts the outcome or
opens a linked continuation task.

## Continuous GC integrated with the model

“GC” here is not a last-minute compression job. It is the continuous policy
executor that maintains the online working set as the task changes.

### Four distinct collectors

1. **Event maintenance (minor GC)**
   Applies new lifecycle intents, consumes ephemerals, updates attention,
   handles scope transitions, and refreshes roots on every relevant runtime
   event.
2. **Episode collection**
   At an episode boundary, promotes the sourced outcome and unresolved state,
   then releases completed Focus/Tool detail.
3. **Resident/reconciliation GC**
   Mark roots, age generations, sweep into reversible residency, externalize
   overflow, and reactivate justified evidence. Run at safe runtime boundaries
   and by explicit `context.collect`, never because a token threshold fired.
4. **Storage GC**
   Permanently deletes only retention-expired, semantically dead evidence with
   no live strong references and no audit/pin/durable requirement.

The first three change model visibility/residency. Only the fourth destroys a
stored body.

### Event pipeline

```text
UserInput
  -> ingest record and bounded entities
  -> detect/confirm episode transition
  -> minor GC dirty set

BeforeModel
  -> expire turn-scoped leases/ephemerals
  -> apply dirty lifecycle transitions
  -> preview materialize from TaskAnchor + Focus + justified live evidence
  -> PromptAssembler + final provider token packing
  -> after a successful non-stale ModelOutput, commit ContextConsumptionAck
     for the exact item/ref ids in that operation's final frame;
     refusal/failure/cancellation/stale completion commits no ack

AfterTool operation commit
  -> emit bounded WorkingSetSignal (ids/entities/status only)
  -> update hot roots before the next model round
  -> keep large body in TurnFrame/artifact

AfterModel / turn commit
  -> persist admitted observations and assistant result
  -> minor GC
  -> bounded GC step / reconciliation report

FocusChanged / TaskCompleted / Checkpoint
  -> transactional scope/lifecycle maintenance
  -> checkpoint-safe reconciliation
```

The current code already implements most trigger names. The important delta
is that `AfterTool` signals must affect the next model round, and each pass
must have bounded work rather than repeatedly scanning all retained history.

### Root priority

Do not use one overloaded notion of “root.” The current Durable/Pinned/root
rules conflate three decisions that need independent reasons:

| Protection | Meaning | Typical examples |
| --- | --- | --- |
| Mandatory materialization | Must be represented in the next model request | Active TaskAnchor; current user turn; runtime policy envelope |
| Online residency | Keep the referenced body in the fast working set | Current Focus, unresolved open loops, active error/evidence, open Tool scope, bounded lease |
| Storage retention | Body may be Cold/External but permanent deletion is forbidden | Suspended-task anchor refs, final output, CompletionRecord evidence/artifacts, audit pin |

A record may be a storage root without being Resident or model-visible. This
is the required semantics for completed-task output.

Default execution value should fall approximately as follows:

```text
TaskAnchor and hard user constraints
  > current Focus and unresolved open loops
  > current episode evidence/errors/decisions
  > explicitly leased/pinned live records
  > verified outcomes of recent closed episodes
  > older sourced trajectory descriptors
  > old raw dialogue/tool bodies
```

Membership in the active Task alone is never a root. A strong causal edge,
unresolved status, explicit protection, or current focus signal is required.
Terminal semantic state excludes an item before pin/score-based selection;
audit retention can preserve its body without making it model-visible.

The runtime should pass typed reasons rather than asking a score to infer
authority:

```text
RootReason = TaskAnchor
           | CurrentEpisode
           | OpenLoop
           | HardConstraint
           | ActiveError
           | CompletionPending
           | CompletionEvidence
           | StrongDependency
           | ExplicitLease
           | AuditPin
```

Task-state root transfer:

| Task state | Mandatory prompt | Online roots | Storage roots |
| --- | --- | --- | --- |
| Active | TaskAnchor | current Episode/Focus, open loops, active constraints/evidence, open Tool frames | Anchor evidence/artifact refs |
| Suspended | none in another task | normally none except explicit policy | anchor refs required to resume |
| Completing | frozen TaskAnchor + completion status | current Episode, exact output, acceptance/verification evidence | the same set until commit/rollback |
| Closed | none by default | none by task membership | CompletionRecord, final output, deliverables, verification/audit refs |

Tool outputs are comparatively easy because Tool scopes already have explicit
open/close identities. The final request now acknowledges the exact included
ids, but the current next-model-round scope close is still a scheduling
heuristic rather than an ack-driven root transfer. Dialogue and reasoning
records now share exact frame consumption; they still need obligation-driven
promotion:

- Goal/Constraint/AcceptanceCriterion/UserDecision patch TaskAnchor;
- OpenLoop/Blocker stays online until resolved, delegated, or transferred to
  an outcome;
- ordinary User/Assistant narrative stays episode-local and becomes
  collectible after consumption;
- a verified Decision/Finding/Artifact/Evidence ref may promote across
  episodes;
- the final Assistant result is promoted by the completion transaction, not
  by recency/importance score.

### Bounded incremental work

Event-driven triggering is necessary but not sufficient. The current minor
pass iterates the entire Resident heap, and full GC can still scale with total
history. Replace this gradually with:

- a dirty-id queue for new/changed records and lifecycle intents;
- indexes from task/scope/entity/strong edge to candidate ids;
- a small aging cursor or generation work queue per event;
- explicit per-pass item and I/O budgets;
- a revision/base-revision check before plan/IO/commit;
- bounded store concurrency and resumable pending work;
- a full invariant-reconciliation mode for checkpoint/startup/operator use.

Deferring unfinished work to the next event is normal. It must be observable
and must not let a semantically dead item re-enter a prompt.

Do not overload existing `Generation::{Nursery, Working, Stable}` with Task or
Episode meaning. Generation is collection age; Task/Episode/Focus are scope
and authority. Keep those dimensions orthogonal.

### Summary creation and GC

Summarization is a derivation pass attached to lifecycle boundaries, not a
token-overflow handler:

- derive an EpisodeOutcome when an episode closes;
- optionally refresh TaskAnchor from verified typed patches;
- store exact sources first and retain their references;
- never recursively summarize a summary when source refs are available;
- on derivation failure, keep source roots until a safe retry/fallback;
- account for derivation tokens and latency in evaluation.

GC decides which source bodies stay online after the derived record commits.
It does not ask a summary to prove facts absent from its sources.

## Recall contract (non-vector v0)

Recall is deliberate navigation, not automatic resurrection of everything
that matches a word.

Keep three operations separate:

```text
fetch(ref)              # exact transient read into the current TurnFrame
admit(ref, reason)      # same logical item id joins the working set
derive(refs, card)      # new id with strong DerivedFrom provenance
```

Requirements:

- [x] `search`/`inspect`/`fetch` are transient and do not create a second
  `ToolObservation`; runtime E2E and context-service process parity are
  verified, including real externalization and full-body fetch.
- [x] `admit` and `derive` are now present in the working tree with per-turn
  quotas and a `DerivedFrom` edge; identity, terminal-state, cross-residency,
  storage-root, rollback, and full-suite acceptance remain to be validated.
  **All validated 2026-08-12.** Identity and single-transition admits are
  covered for the external and warm paths, terminal states never resurrect,
  and per-turn quotas bound both directives; new coverage closes the rest —
  rollback (a store blob that vanished between plan and IO makes the admit a
  silent no-op: entry stays external, nothing minted, no pending transition),
  retention authority (a durable item keeps `Durable` when its body moves
  external -> resident), and full-suite acceptance (a scripted
  `context.manage` admit call routes end to end through the runtime directive
  path and the item re-enters the working set under its original id).
- [x] Bound every query, descriptor, fetched excerpt, full-body result, and
  model-facing output; spill large bodies to one artifact.
  **Bounded 2026-08-12.** `context.search` limits are clamped to
  `CONTEXT_SEARCH_MAX_LIMIT` in execution (0 keeps the engine default) and
  the free-text query is truncated to `CONTEXT_SEARCH_MAX_QUERY_CHARS` chars
  before it reaches the engine; search hits render bounded summaries, and
  `context.fetch` full bodies pass through the trusted output broker, which
  caps every model-facing field and spills oversized content to one
  artifact before the model sees a truncated middle.
- [x] Record access without promoting evidence into a trusted prompt role.
  **Satisfied 2026-08-12.** Retrieved history, external refs, files and
  tool results render only as low-authority `user`/`tool` observations,
  never as `system` (prompt.rs keeps policy in the system layer; the
  injected-instruction and external-refs tests pin this). `fetch` is a
  transient read that never re-enters the working set, `admit` is an
  explicit promotion that still renders as ordinary context, and access
  recording only reinforces recency — it never changes the item's prompt
  role.
- [x] Distinguish “evidence does not exist” from “not found by this search.”
  **Done 2026-08-12.** An empty search result now distinguishes the two
  cases in the model-facing message: with no kind/scope/task filter it
  reports that no externalized items match, with a filter it says nothing
  matches within the requested filter and evidence may exist under a
  different filter — so the model can decide to give up or retry under
  another filter instead of misreading a filter miss as absent evidence.
- [~] Require source authority/taint checks again at fetch/admit time.
  **Precondition landed 2026-08-12.** `ExternalizedContext` now carries
  the item's `source` authority captured at externalize time, and the
  `inspect` catalog projection reports the real source instead of a fixed
  "externalized" placeholder — so the source of an externalized item is
  visible without a store read and survives the external -> resident move
  on admit (blob reads already carried it). The kernel's retrieval output
  now renders the authority end to end: `context.search` hits, `inspect`
  metadata and the `fetch` header all show `source=...` (`-` when absent),
  so the model and audit can see where every retrieved item came from.
  The actual authority/taint check policy at fetch/admit time (what a
  given source may or may not do on re-entry) remains open.
- [ ] Keep vector retrieval deferred as an optional candidate provider; it
  may suggest ids but cannot own lifecycle truth or bypass admission.

## Unified runtime input and discovery

The next design slice should unify the *mechanics* of user input, tool
results, collaborator results, runtime events, and deliberate recall without
flattening their authority. A user message is tool-like in the sense that it
arrives asynchronously, has a causal id, changes the next execution frame,
is consumed exactly once, and eventually becomes collectible evidence. It is
not an ordinary `ToolOutput`: only the user source may directly change the
task goal/scope, interrupt work, revoke a standing grant, or override a plan.

Target input pipeline:

```text
raw input
  -> bounded RuntimeInputEnvelope(source, authority, task, causal parent)
  -> typed interpretation / StatePatchProposal
  -> RuntimeActor validates revision, policy, and source authority
  -> Applied | Queued | Rejected | InterruptCommitted
  -> exact consumption acknowledgement
  -> episode evidence / GC lifecycle
```

The first taxonomy should cover normal dialogue, goal/constraint/priority
patches, task cancellation or steering, artifact/evidence submission, and
permission revocation. Model interpretation may propose a patch, but Runtime
owns the commit. Tool or collaborator prose can propose evidence and open
loops; it cannot impersonate a user patch merely because all sources share an
event envelope.

Discovery should likewise be federated rather than implemented as one new
authoritative memory database:

```text
runtime.search(query, kinds, limits)
  -> bounded ResourceDescriptor[]
runtime.inspect(ref, revision)
  -> bounded metadata/provenance
runtime.resolve(ref, revision, range)
  -> transient body/schema/handle
explicit admit | surface | invoke
```

Candidate kinds are `Context`, `Tool`, `Artifact`, `Task`, `Agent`, `Skill`,
`Capability`, and `Event`. Each provider keeps its existing source of truth;
the shared layer standardizes stable typed refs, bounded descriptors,
authority/taint, lifecycle, source revision, freshness, permission needs, and
load cost. Search is read-only: it never admits context, loads a tool, starts
an agent, grants permission, or mutates TaskAnchor by itself.

Next tasks:

- [ ] **CTX-DISC-01** Define the bounded, versioned resource-ref/descriptor
  contract and distinguish `not found` from `provider unavailable`, stale
  revision, denied, and exact evidence absence.
- [ ] **CTX-DISC-02** Prototype non-vector federated search over the existing
  context and capability/tool providers; keep provider-owned indexes and
  deterministic ranking before adding artifacts/tasks/agents.
- [ ] **CTX-DISC-03** Enforce `search -> inspect/resolve -> explicit
  admit/surface/invoke`; record every transition and cap query count, fanout,
  rows, bytes, tokens, latency, and repeated-search loops.
- [ ] **CTX-EVENT-01** Generalize the current user-message path into a typed
  input envelope plus source-authorized state proposals while preserving the
  current direct, deterministic cancellation and command paths.
- [ ] **CTX-EVENT-02** Give input records an explicit event lifecycle:
  `Received -> Interpreted -> Applied/Queued/Rejected -> Consumed ->
  Archived`; interruption and supersession must be revision-fenced and
  replayable.
- [ ] **CTX-EVENT-03** Replace the current full-content
  `UserMessageAccepted` audit payload with a bounded preview plus stable body
  ref, digest, size, authority, and task/turn ids. Store the exact body once
  in the evidence plane and budget its model projection separately; event
  logging must not become an unbounded duplicate of the transcript.
- [ ] **CTX-GC-10** Couple search/resolve signals to bounded access
  reinforcement and GC explanations, but never let a search hit override
  terminal semantic state or mandatory TaskAnchor roots.

## Multi-agent context

The topology is one authoritative coordinator plus bounded collaborators, not
N independent transcript memories merged together.

- The main runtime owns TaskAnchor and completion criteria.
- The model-facing control may look tool-like — bounded
  `agent.search/inspect/spawn/send/wait/cancel/status/collect` operations —
  while the Runtime treats each child as a leased, asynchronous execution
  resource rather than a one-shot function call.
- A collaborator receives a scoped `AssignmentCard`: objective, constraints,
  acceptance condition, allowed evidence refs, budget, and child scope.
- It returns a `HandoffCard`: status, findings, decisions, artifacts,
  verification, unresolved items, and exact evidence refs.
- The coordinator validates and admits the handoff; collaborator prose is
  evidence, not automatic TaskAnchor authority.
- Full collaborator output is stored once as an artifact/evidence body. The
  coordinator prompt gets the bounded card plus refs.
- Closed collaborator scopes follow the same episode collection rules.
- A child inherits a narrower permission set, context view, token/tool/time
  budget, deadline, and cancellation generation. It can never widen its own
  authority or commit directly to the parent's TaskAnchor, CompletionRecord,
  approval policy, or evaluation Core.
- Parent TaskAnchor open loops/root claims keep a delegated assignment alive;
  accepted handoff refs replace those roots at completion. Cancelled, failed,
  or abandoned children retain only the bounded evidence needed for diagnosis
  and audit.

Keep the collaborator run lifecycle independent from tool/catalog lifecycle:

```text
Allocated -> Starting -> Running <-> Waiting
          -> Completed | Failed | Cancelled | Expired
          -> Collected/Archived
```

The `RuntimeActor` remains the sole parent authority and lifecycle owner. A
worker may execute a scoped agent loop through the runtime implementation, but
it is not a peer orchestrator allowed to mutate shared parent state.

This can be built after the single-agent TaskAnchor/Episode contract is
stable and after effect/sandbox/evaluation gates. It should not introduce a
second authority or another transcript-based context system.

## Ordered implementation queue

### Phase 0 — Freeze and measure the current baseline

- [x] Keep event-triggered maintenance and turn-boundary GC independent of
  token pressure.
- [x] Bound the long-task Resident set with Focus-scope episode rotation
  (`CTX-01`) and retain the 10,000-turn regression.
- [x] Make short-term lifecycle mutations consistent across body locations
  (`CTX-02`).
- [x] Replay fact comparison now uses independent fresh engines for cost and
  coverage, with a regression matching the comparison result to a standalone
  fresh coverage run.
- [x] Rename/split Resident-only diagnostics from real logical catalog totals
  before using `total_items`, `inspect`, or replay `final_total` as retention
  evidence. **Landed via `CTX-09` (catalog DONE):** `total_items`/`inspect()`
  are the logical catalog (resident heap + warm buffer + external store
  entries; each id has exactly one owner, so the sum is exact) and replay
  `final_total` is a real catalog total — `context-simple/src/diagnostics.rs`
  computes the split and the external map maintains Cold/External counts in
  O(1).
- [x] Record baseline Resident/Warm/Cold/External counts, candidate count,
  selected count/tokens, maintenance work, GC work, store I/O, materialize
  p50/p95, recall count, and task success.
  **Counts/tokens landed 2026-08-12** — `agent-eval` `RunMetrics` now
  aggregates materialize rounds, cumulative selected items/tokens,
  cumulative `approx_active_tokens` and the final Resident/Warm/Cold/
  External snapshot from `ContextPrepared`, and the cross-engine
  comparison table prints them per engine (measured on the fixtures:
  dynamic selects 20 items / 380 active tokens vs 48 / ~1 250 for
  append/rolling on the same workload, with a 9-resident + 3-warm final
  working set).
  **Store I/O + latency + recall landed 2026-08-12** — `ContextGcReport`
  carries `store_write_bytes` / `store_read_bytes` / `store_recalled_items`
  (filled by `commit_full_gc` from the externalization/recall bodies),
  `ContextPrepared` carries `materialize_ms` (the engine's own materialize
  call, timed by the runtime before rendering overhead), and `RunMetrics`
  aggregates cumulative store I/O plus nearest-rank `materialize_ms_p50` /
  `p95`; the comparison table prints them per engine. Recall count is the
  store recall side of full GC (`store_recalled_items`).
- [x] Replace the placeholder “summary” evaluation arm with a real rolling
  summary baseline and count all manager/derivation tokens.
  **Done 2026-08-12.** `context-baselines` gained a `Summarizer` trait
  (injected via `RollingSummaryEngine::with_summarizer`) plus a bounded
  fold-digest (`SUMMARIZER_PRIOR_CAP = 2 000` chars), so the rolling marker
  reflects the folded content instead of a constant placeholder; the eval
  harness injects a deterministic `ScriptedSummarizer` and the cross-engine
  comparison now folds on the fixture workload (the default 9 000-token
  threshold would never fire — the whole run stays near 300 tokens — so the
  rolling arm uses 200/100 thresholds and folds from the fourth turn).
  Manager/derivation cost is counted separately from the input-token gap:
  `manager_token_cost` re-materializes the final state and sums the
  `Summary`/`source == "derived"` items, surfaced as `EngineRun.
  manager_tokens` in the comparison table (measured: rolling folds 3-4
  records with a 26-token marker at ~12 960 model_in vs append ~13 220 vs
  dynamic ~12 090 on the same five-turn script; append/dynamic inject zero
  manager tokens — the fixtures never complete a task or derive).

### Phase 1 — Correctness before smarter policy

- [x] Verify the `TaskToolRequirementSet` first slice:
  actor-serialized whole-set CAS, `MustSurface`/`PreferSurface`/`KeepReady`
  round planning, bounded decision events, runtime surface revision, and
  RuntimeCheckpoint v2 restore/resume. Full workspace tests and strict Clippy
  pass. This item is a tool-demand subset and
  must not be used as evidence that the complete TaskAnchor exists.
- [x] Make live-restore rebasing a durable, bounded audit transaction
  (`CORE-03`): `RuntimeEvent::RuntimeRestored` carries typed
  old/restored/effective revision data and a capped rebased-task sample;
  an audit failure after restore commit leaves aligned restored state but
  sets `RecoveryRequired` and fences further mutation.
- [x] Replace free-form task completion Summary with a typed `TaskOutcome` /
  `CompletionRecord` carrying task id, anchor revision, exact final-output
  ref/digest, and bounded artifact refs (`CTX-10`). Acceptance results,
  verification and unresolved state remain anchor fields the runtime has not
  yet sourced autonomously.
- [x] Persist the exact final response before ContextItem truncation; stop
  treating task-less Session Durable summaries as the authoritative result.
  The completion summary is now a typed task-owned record with a verifiable
  digest, but the raw final assistant response is still truncated before
  ContextItem — raw-evidence retention stays open.
  **Done 2026-08-12.** The actor writes the *full* final assistant response
  to an artifact (`state_dir/artifacts/<run>/assistant-response-<uuid>.txt`)
  before the bounded ContextItem is built, when the composition root wired
  an artifact workspace (`RuntimeServices.artifact_workspace`); a
  persistence failure aborts the turn commit
  (`TurnCommitPhase::AssistantMessageArtifact`). `commit_completion`
  attaches that ref to the task's `CompletionRecord` (deduplicated against
  the model's self-declared list), so the complete raw output is reachable
  from the typed record even when the model named no artifacts. Pinned by
  `final_assistant_response_is_persisted_in_full_before_
  contextitem_truncation` (a 40k-char response survives intact) and
  `completion_record_attaches_the_raw_final_response_artifact` (the
  CompletionRecord carries the ref end to end).
- [x] Extend the now-checkpointed tool-demand subset into the bounded complete
  TaskAnchor as the only task-authority owner; add typed CAS patches for goal,
  constraints, criteria, progress, open loops and evidence plus completion
  refs. Do not duplicate this authority in `FocusState` or ContextEngine.
- [x] Make Anchor plan/focus/open-loop/criteria/evidence patches autonomous by
  default; encode the few goal/scope/waiver conflicts that require a boundary
  escalation instead of per-step confirmation.
  **Done 2026-08-12.** `AnchorPatch` is a bounded field-level patch
  (serde names mirror `TaskAnchor`, applied through one CAS against
  `base_revision`) and the authority split is explicit policy:
  interpretation/plan/open-loops/criteria/refs are `Autonomous` and apply
  directly, while goal/constraints are `Boundary` and must clear the
  approval gate first (`PatchTaskAnchor` presents them as a synthetic
  `task.anchor` tool call so existing approval policies and the v2 shadow
  gate see a typed request; a deny errors out and leaves the anchor
  untouched). `TaskAnchorChanged` now carries the `patch_kind` label, and
  whole-anchor replacements are labeled by the same field split. Unit
  tests cover classification/apply/CAS, E2E covers autonomous-lands,
  boundary-approved and boundary-denied under `PolicyApprovalGate`. The
  model-facing `task.anchor` tool entry point remains (queued with the
  canonical-catalog work).
- [x] Implement atomic completion root transfer: the context engine records
  the completed task and closes its scopes first (rollback on failure), then
  the `TaskManager` commits status + outcome; fault injection proves no
  half-closed task and an audit gap after commit fences recovery. Explicit
  `CompletionPrepared -> CompletionCommitted` phase *events* are not emitted
  as separate named stages; the ordering and atomicity are tested directly.
- [x] Reset/replace episode-local generation when Focus scope rotates; add a
  test that an overlong episode rotates once and the next episode receives a
  fresh turn budget. **Landed 2026-08-12:** `scope::close_focus_episode`
  resets `FocusState.generation` to 0 at rotation (a rotated episode starts
  with a fresh budget), and
  `one_overlong_episode_does_not_exhaust_later_episode_budgets` drives an
  episode past `episode_max_user_turns`, asserts the guard fires at the
  budget boundary (not immediately), then verifies five related messages in
  the next episode do not rotate and stay resident.
- [x] Apply TTL/terminal aging coherently across every body location and keep
  full GC progressing in external-only state.
  **Landed:** ephemeral TTL/staleness age in user turns wherever the item
  lives — resident heap (`residency.rs`) and the warm reversible buffer
  (`gc/minor.rs`: "ephemeral TTL expired in the warm buffer" / "stale in
  the warm buffer", tombstoned once dead); externalized (`Cold`) entries
  age to `External` by full-GC generations (`gc_external_ttl_generations`,
  `store::age_external_entries`), and a full GC whose heap and buffer are
  empty still runs and ages external entries (`gc/full.rs`: "An
  external-only state must ...") — so aging never stalls when nothing is
  resident. Storage GC deletes only entries whose semantic lifecycle ended
  `storage_ttl_ticks` ago (`store::plan_storage_gc`). Regressions cover
  the external-only path (`gc_external_ttl_generations: 1` scenario) and
  the warm-buffer TTL.
- [x] Make dependency roots and scope-close transitions location-independent;
  surface tool-scope close transitions/errors as mandatory audit events.
  **Landed:** supersession, error-verification and dependency scans cover
  the heap, the warm buffer and the external map (`gc/reachability.rs`:
  "the scan covers the heap, the warm buffer and the external map"), so a
  decision that was evicted and externalized is still the same decision
  and still gets superseded/verified anywhere its body lives; the target
  may live in any body location ("Lifecycle authority must not [depend on
  location]"). Tool-scope close publishes its transitions and failures as
  audit events (`CTX-06`: `tool_scope_close_publishes_its_transitions`,
  `tool_scope_close_failure_is_published_as_an_error`).
- [x] Verify transient search/inspect/fetch disposition and process-service
  parity (`CTX-03`).
- [x] Implement bounded `admit`/`derive`: admit preserves the source identity
  and single ownership; derive creates a new item with `DerivedFrom`; both
  have per-turn quotas and runtime E2E tests. Canonical catalog and
  provenance/authority admission remain separate work.
- [~] Introduce canonical `ContextCatalog` ownership; body movement changes
  location only, never lifecycle authority (`CTX-02` structural target).
  **Authority-isomorphism step landed 2026-08-12.** `ExternalizedContext`
  now carries the item's full authoritative lifecycle metadata (importance/
  relevance, real `created_tick`/turn clocks, access count, GC generation,
  eviction tick) captured at externalize time — externalization no longer
  degrades authority, and `inspect` projects the real values instead of
  zeros or the externalization tick. Body movement no longer rewrites
  authority: `reenter_working_set` stopped clobbering `created_tick`/
  `created_turn` on admit, matching the GC reactivate path. The single
  `item_id -> ContextRecord` storage directory (merging heap / warm buffer
  / external map) remains the open structural step.
- [x] Make externalization/recall crash-safe and restart-reconcilable, with
  one owner per blob, checksum/revision, atomic writes, and bounded I/O
  (`CTX-04`).
- [x] Add strong edge kinds and make Storage GC root/traverse every
  non-deletable record (`CTX-05`).
- [x] Serialize or revision-check GC/storage-GC/checkpoint/restore plans and
  validate all residency layers (`CTX-06`).
  **Done 2026-08-12.** The `op_gate` serializes the multi-phase/whole-state
  operations — GC, storage GC, store reconcile, checkpoint and restore —
  so a plan always commits against the state it was planned against
  (`multi_phase_operations_are_serialized_by_the_operation_gate` holds the
  gate and proves every sibling blocks until release), and
  `checkpoint::validate` runs before a restore becomes live: cross-location
  ownership across the heap / eviction buffer / external map, scope
  ancestry and item scope references, with
  `restore_rejects_checkpoints_that_violate_structural_invariants` covering
  every layer including the external map (heap↔external, buffer↔external,
  and duplicate ids inside the map).
- [x] Propagate failures from `BeforeModel` maintenance audit and explicit
  `context.collect`; a context mutation cannot silently outrun its journal.
  **Landed via `CTX-09` (audit propagation DONE):** a failed
  `ContextMaintained` (BeforeModel) publication fences the turn (Error
  event, model never called, no `TurnCompleted`); an explicit `collect`
  propagates both a refused GC pass and a failed `ContextGc` publication
  as `Error` events. Regressions:
  `before_model_audit_failure_fences_the_turn`,
  `collect_audit_failure_is_not_silent`.
- [x] Split materialization preview from model consumption (`CTX-07`). After
  PromptAssembler and final provider packing, a successful non-stale model
  operation acknowledges the exact selected item/external-ref ids and
  materialization revision. Trimmed, refused, failed, cancelled and stale
  rounds receive no reinforcement; reinforcement + the bounded audit event
  roll back together on failure. Remaining CTX-07 packing/hot-path work stays
  open below.
- [x] Render untrusted historical/retrieved content in a lower-authority,
  delimited prompt channel (`CORE-05`). **Closed 2026-08-11:** the
  `PromptAssembler` renders every observation as a low-authority `user`
  message (never `system`); system holds policy only, and retrieved
  file/tool/store content cannot gain system precedence. Regressions:
  `retrieved_history_never_renders_as_system`,
  `injected_instructions_cannot_gain_system_precedence`,
  `external_refs_render_as_low_authority_observations`,
  `malicious_file_and_tool_content_stays_in_the_tool_role`.
- [ ] Coordinate with Effect Runtime/Resource Policy on task-scoped standing
  grants, `DenyAndContinue`, batched boundary requests and interruption caps;
  do not solve approval fatigue by silently broadening permissions.

### Phase 2 — Task/Episode state and continuous incremental GC

- [ ] Pass a bounded active `TaskAnchorView` and typed root claims through
  materialization/GC without duplicating TaskManager authority.
- [ ] Split mandatory-materialization, online-residency, and storage-retention
  roots; report `anchor_revision + source_field + RootReason` for each root.
- [ ] Implement Active/Suspended root downgrade and precise rehydration from
  TaskAnchor/ResumePoint without restoring the old transcript.
- [ ] After Anchor/root-claim properties pass, replace “entire active Focus
  subtree is a root” with Anchor claims + unresolved obligations + a bounded
  recent/TurnFrame lease, so continuous GC also works inside a long episode.
- [ ] Formalize Focus `ScopeId` as the v0 episode identity and emit episode
  open/continue/close reasons.
- [ ] Add sourced `EpisodeOutcome` and atomic `ClosePending -> Closed`
  transition before roots are released.
- [ ] Keep lexical overlap and turn cap as fallback guards; add explicit
  runtime/subgoal/verification boundary signals and measure false rotations.
- [x] Separate `event_seq`, `user_turn`, `gc_epoch`, and
  `last_selected_turn`; every TTL/rule names one clock (`CTX-09`).
  **Landed:** `event_seq` (monotonic, never advanced by `materialize`),
  `turn` (user-turn clock), `gc_epoch` (full-GC generation) and
  `last_selected_turn` (stamped on consumption acknowledgement) are
  separate and every rule names its clock; the consumed-ephemeral check
  uses event distance, ephemeral TTL ages in user turns, recency reads
  `last_selected_turn`. Regressions:
  `materialize_preview_is_a_read_that_advances_no_clock`,
  `selection_stamp_is_written_only_by_consumption_ack`,
  `ephemeral_ttl_counts_user_turns_not_events`.
- [ ] Replace whole-heap minor scans with dirty ids + bounded aging work.
- [ ] Add GC work/item/I/O budgets, backlog metrics, revision fencing, and a
  full reconciliation mode.
- [x] With the `CTX-04`/`CTX-05` safety baseline now closed, schedule bounded
  Storage GC at an
  explicit retention/checkpoint/task boundary and emit its report; never put
  destructive storage deletion on the per-model hot path.
  **Landed 2026-08-12.** Task completion is the explicit boundary: right
  after the post-completion full GC, the actor runs one conservative
  `ContextEngine::storage_gc` pass (`RuntimeServices::context_storage_gc`)
  and publishes the report as a `RuntimeEvent::StorageGc` event — the only
  permanent-deletion surface, never on the per-model hot path, and a
  failure is surfaced as an `Error` event without undoing the completed
  task. The TUI surfaces the report (scanned/deleted/io-errors) when
  anything was deleted. E2E:
  `task_completion_schedules_storage_gc_and_publishes_the_report`.
- [ ] Emit a bounded metadata-only `WorkingSetSignal` at tool commit so the
  next model round sees newly hot files/symbols (`CTX-08`).
- [ ] Bound materialization candidates and external preview tokens; fix
  fit-before-top-K and report candidate/materialize costs (`CTX-07`).

### Phase 3 — Navigation, trajectories, and handoff

- [x] Land the bounded `admit`/`derive` baseline with preserved identity,
  `DerivedFrom` provenance, quotas and runtime tests. Future work is authority
  grading, richer provenance and canonical-catalog ownership, not the basic
  operations.
- [ ] Introduce descriptor cards and stable content-digested `body_ref`s.
- [ ] Produce Task/Episode/Decision/Finding/OpenLoop/Artifact/Evidence cards
  from typed events; never infer runtime authority from prose alone.
- [ ] Add task-completion trajectory distillation with verification and
  source refs; do not inject full old trajectories by default.
- [ ] Add AssignmentCard/HandoffCard after single-agent episode semantics
  pass evaluation.
- [ ] Define independent storage retention profiles for coding, research,
  general assistant, and audit-sensitive runs.
- [ ] Land `CTX-DISC-01..03` over Context + Tool only; compare separate
  `context.manage`/`capability.manage` surfaces with one merged discovery
  surface before replacing either public contract.
- [ ] Land `CTX-EVENT-01..03`: typed user-input lifecycle, bounded event
  bodies, and source-authorized state patches. Do not route user authority
  through `ToolOutput`.
- [ ] Specify the managed-child lifecycle and `agent.manage` surface, but keep
  spawning disabled until the effect, sandbox, resource, and real-evaluation
  gates can enforce it.

### Phase 4 — Evaluation-gated tuning

- [ ] Compare at least:
  - full/sliding transcript;
  - real rolling-summary compaction;
  - summary + immutable raw pointer;
  - structured TaskAnchor/Episode cards + raw refs + continuous GC.
- [ ] Measure task success, goal/constraint retention, required-fact recall,
  stale/terminal contamination, abstention, exact-evidence fidelity, total
  tokens (actor + manager + summary + recall), latency, I/O, and variance.
- [ ] Run long coding tasks with tool loops, errors/fixes, scope changes,
  restarts, collaborator handoffs, and deliberately old-but-required facts.
- [ ] Only tune scoring/rotation thresholds after observability shows which
  explicit signals are insufficient.

## Acceptance properties

### Long-task boundedness

- Resident bytes and candidate count flatten with current episode plus live
  unresolved state across 10,000+ turns.
- Preserve the existing 10,000-turn checks (Resident peak below 200 and
  turn-2,000-to-10,000 growth no greater than 20) while adding bytes and
  candidate work; these numeric thresholds describe the current fixture, not
  a universal product limit.
- Minor-GC work per ordinary event is bounded by configured work budget, not
  total retained evidence.
- After warmup, last-quintile materialization p95 is initially targeted at no
  more than 1.10x the first stable-quintile p95; revise only with measured
  workload evidence.

### Goal and completion quality

- Task goal, hard constraints, acceptance criteria, and unresolved open loops
  remain available at every `BeforeModel` boundary.
- Completion cannot be declared without the current TaskAnchor acceptance
  criteria and verification evidence.
- Every Closed task has exactly one committed CompletionRecord; every
  CompletionRecord names its task, frozen anchor revision, final turn, and
  exact output digest/ref.
- The final user-facing output is byte-for-byte readable after completion,
  buffer overflow, restart, and Storage GC; `/done` prose is never used as an
  independent replacement body.
- Task success is not lower than the best transcript/summary baseline at a
  comparable total inference budget.
- Before claiming non-inferiority, use paired coding runs (proposed minimum:
  30 tasks x 3 runs) and pre-register a success-rate-difference lower bound
  (initial proposal: 95% interval lower bound no worse than -5 percentage
  points). A 3-run paired smoke test is enough only to validate the harness.

### Lifecycle safety

- Terminal items never appear in materialized context, external preview,
  fetch/admit results, or derived live cards.
- Episode close either commits outcome + TaskAnchor patch + root release, or
  leaves the old episode recoverable; no half-close state.
- Completion fault injection at output write, prepare journal, validation,
  outcome commit, scope close, and TaskManager commit yields either a fully
  recoverable Active/Completing task or a fully Closed task—never a half-close.
- Active -> Suspended -> unrelated work/GC -> Active restores the exact Anchor
  revision, ResumePoint, constraints, criteria, and open loops without
  automatically restoring ordinary old dialogue.
- Fetch/search/inspect leave logical item ids/count unchanged; admit preserves
  the id and emits exactly one explainable transition.
- Completed-task evidence never reactivates from entity heat alone.

### Evidence and storage integrity

- Every Stored record has exactly one readable, digest-matching body and every
  managed blob has one catalog owner after restart reconcile.
- Permanent deletion cannot remove a body reachable by a strong edge from any
  non-deletable record, regardless of residency.
- Crash/cancel injection at every plan/write/rename/commit/delete point leaves
  a recoverable state.
- CompletionRecord/final output/deliverable/verification strong edges remain
  Storage roots without making the completed task a Prompt/Resident root.
- Completing 1,000 small tasks does not make Resident or materialization
  candidate counts grow linearly; old outcomes remain explicitly searchable
  by task id and exact output ref.

### Explainability and authority

- Every model-selected item says why it was admitted and selected.
- Every attention/residency/semantic change names trigger, clock, revision,
  cause, and related id where applicable.
- Untrusted evidence never gains System authority through selection, summary,
  collaborator handoff, or recall.

## External evidence and limits

External work supports the general architecture but does not validate this
project's exact roots, thresholds, episode boundaries, or GC schedule:

- [Anthropic, Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
  supports iterative curation, just-in-time identifiers, bounded working
  memory, and clearing old raw tool results.
- [MemGPT](https://arxiv.org/abs/2310.08560) supports tiered virtual context
  and explicit page-in/page-out, while retaining a recent-event queue and
  using token pressure differently from this project.
- [CoALA](https://arxiv.org/abs/2309.02427) supports separating working,
  episodic, semantic, and procedural memory and compiling each model input
  from a working-memory subset.
- [LongMemEval](https://arxiv.org/abs/2410.10813) motivates update-aware,
  temporal/session-structured memory and tests stale facts plus abstention.
- [HORMA](https://arxiv.org/abs/2606.11680) and
  [MemWalker](https://arxiv.org/abs/2310.05029) support hierarchical notes
  that navigate back to raw evidence; both also expose summary/navigation
  error as a real failure mode.
- [Agent Workflow Memory](https://arxiv.org/abs/2409.07429) supports
  distilling reusable workflows from successful trajectories rather than
  replaying raw trajectories.
- [AgentPoison](https://papers.nips.cc/paper_files/paper/2024/file/eb113910e9c3f6242541c1652e30dfd6-Paper-Conference.pdf)
  shows that retained/retrieved memory remains a poisoning surface, so source
  authority and read-time isolation are required even for cold evidence.

These sources justify testing the direction. They do not justify claiming
that event-driven GC is superior until coding-agent A/B/C evaluation counts
all tokens, latency, failures, and run-to-run variance.

## Explicitly deferred

- Vector databases, embedding retrieval, learned selection, and cross-session
  memory are deferred until the non-vector dynamic working set is measured.
- Automatic reusable workflow/skill promotion is deferred until trajectories
  are verified and versioned.
- Model-only episode-boundary decisions are deferred until deterministic
  signals and their errors are observable.
- Permanent “keep all raw evidence forever” is not a product invariant.
  Context GC is non-destructive; Storage GC must later implement retention,
  privacy/deletion, audit, and poisoning policies.

## Next discussion gates

1. Ratify the source-authority table: which user inputs commit directly,
   which need typed interpretation, and which tool/agent proposals can only
   become evidence.
2. Ratify `ResourceRef`/`ResourceDescriptor`, provider revision semantics,
   and the hard limits for one federated search round.
3. Decide whether v0 adds a new `runtime.search` surface or first implements a
   shared internal planner behind existing `context.manage` and
   `capability.manage` controls.
4. Ratify the independent catalog/schema, invocation/effect, host-process,
   and managed-child lifecycles.
5. Ratify child budget inheritance, cancellation, parent root transfer, and
   the minimum AssignmentCard/HandoffCard fields.
6. Choose per-event GC/search work budgets and acceptable deferred-work
   backlog semantics.
7. Extend M15 with search precision/cost, stale-resource rejection, steering
   latency, child cancellation, handoff fidelity, and parent task-success
   metrics before enabling broad multi-agent execution.

Implementation may begin with the bounded read-only Context + Tool discovery
prototype and typed user-event envelope. Managed-child execution remains
gated on trusted effects, sandboxing, resource policy, and real evaluation.
