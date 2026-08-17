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
| Runtime triggers | Implemented | `ContextMaintenanceTrigger` covers `UserInput`, `BeforeModel`, `AfterModel`, `AfterTool`, `FocusChanged`, `TaskCompleted`, and `Checkpoint` in `crates/agent-contracts/src/context.rs`. `RuntimeActor` invokes these paths in `crates/agent-runtime/src/actor/`. |
| Continuous collection | Implemented baseline | `maintain()` runs lifecycle maintenance at runtime events, and `finalize_turn()` runs full `context_gc()` after every committed model turn. `context.collect` can request an additional pass. There is no token-limit trigger. |
| Orthogonal lifecycle axes | Implemented | `ContextItem` separates attention, semantic state, physical residency, retention, and GC generation. This is the right base model. |
| Reversible residency | Implemented baseline | `Resident -> Warm -> Cold -> External` moves old bodies through heap, bounded eviction buffer, and filesystem store; full GC reports eviction/reactivation reasons. |
| Scope ownership | Implemented | Session, Task, Focus, and Tool scopes exist. Runtime owns Task/Tool transitions; the engine owns policy within the contract. |
| Episode-bounded working set | Implemented baseline (`CTX-01`) | A Focus scope acts as the current episode container. Low lexical overlap or a 500-user-turn guard rotates it; the episode-local generation resets on rotation, durable/core-labeled results promote, and ordinary dialogue becomes evictable. The 10,000-turn regression keeps Resident roughly flat. Typed episode boundaries and outcomes remain open. |
| Cross-residency lifecycle behavior | Implemented behavior + catalog directory (`CTX-02`) | Terminal transitions, completion/protection handling, TTL aging, dependency scans, and scope-close promotion cover Resident/Warm/Cold/External. `ContextCatalog` is the `item_id -> location` directory across heap / warm buffer / external map; authority metadata stays on the single body. |
| External search/inspect/fetch | Implemented baseline + indexed search + graded access (`CTX-GC-11`) + discovery cards (`CTX-DISC`) + M15 retrieval metrics | Exact non-vector lookup and full-body fetch exist, including process-service parity. Terminal external entries are filtered. `context.search` generates candidates from catalog indexes (kind/scope/task/label/entity); summary/uri needles that hit no key residual-scan. Hits carry `ResourceDescriptor`s; inspect/fetch misses distinguish `not_found` / `evidence_absent` / `provider_unavailable`. Access stamps are graded: search-hit weakest (one Cold-aging delay, per-item cooldown, identical-query budget), inspect/fetch stronger reads, `admit` a residency move, consumption ack the strongest. `agent-eval` meters search recall/latency and found-after-forgotten; that is instrumentation, not a policy change. |
| Transient retrieval result | Implemented baseline (`CTX-03`) + capability search/inspect (`CTX-DISC-03`) | `ToolResultDisposition` keeps context search/inspect/fetch and `capability.manage` search/inspect transient in the current `TurnFrame`; load/unload still persist. Runtime E2E and context-service parity verify that context retrieval does not create duplicate observations. |
| User input envelope | Partial (`CTX-EVENT`) | Dialogue goes through `RuntimeInputEnvelope`. `/focus` `/done` `/cancel` stay direct commands. `UserMessageAccepted` is a 240-char preview; the exact body is sealed as `user-input` when a workspace is wired. Busy dialogue uses a 1-slot in-memory `Queued` (overflow `Rejected`). Cancel publishes `InterruptCommitted` after `TurnCancelled`. Applied → Consumed → Archived on a successful turn. Replay resolves `body_ref` when given a workspace. Dialogue `proposal` is still `None`. |
| Admission/derivation | Partial | `admit` and `derive` operations, quotas and identity/non-duplication tests exist, but authority/taint, richer provenance, storage-root, and canonical-catalog semantics remain open. Do not infer their full lifecycle contract from the closed transient-retrieval defect. |
| Task anchor | Implemented baseline (`CTX-10`) | Each `TaskRecord` owns a bounded, versioned `TaskAnchor` (goal interpretation, constraints, acceptance criteria, plan progress, open loops, typed root claims) with whole-set CAS, a bounded `TaskAnchorChanged` audit event, and RuntimeCheckpoint v4 persistence + restore validation. The tool-demand slice (`TaskToolRequirementSet`) remains its own bounded CAS surface. A prompt `TaskAnchorView` now flows through materialize into the focus frame; typed claims split prompt/residency/storage and report `anchor_revision + source_field + RootReason`. Active/Suspended downgrade and sourced `EpisodeOutcome` remain. |
| Structured episode outcome | Partial | Task completion now commits an immutable typed `CompletionRecord` (task id, anchor revision, summary, final-output ref/digest, artifacts) atomically with the status flip (`CTX-10`). Episode rotation still does not derive a typed, sourced `EpisodeOutcome` per rotated focus episode. |
| Task completion output | Implemented baseline (`CTX-10`) | `/done`/`task.complete` commit one task-owned `CompletionRecord`; `TaskCompleted` carries task/result identity, and completed records are storage rather than residency roots. When an artifact workspace is wired, the actor writes the complete final assistant response before `ContextItem` truncation and attaches its artifact ref to the record. The dedicated `final_output_ref`/digest still identify the bounded completion summary, so richer outcome/evidence fields remain open. |
| Canonical catalog | Implemented navigation directory + incremental dirty sync | `ContextCatalog` is the `item_id -> location` directory plus task/scope/kind/entity/label/lifecycle indexes. Bodies remain in heap / warm / store; GC moves location, it does not copy authority. Checkpoints serialize the three stores and rebuild the directory. Hot-path updates are dirty-id upserts; unmarked length changes still rebuild. |
| Immutable raw evidence | Partial | The final-assistant path stores the full response once as an artifact before bounded context ingest and makes it reachable from `CompletionRecord`. General user/tool/context ingress still preserves only bounded bodies (or tool artifacts where explicitly brokered); a uniform immutable `EvidenceBody` contract is not implemented. |
| Store integrity | Implemented crash-recovery baseline (`CTX-04`) | Atomic write/rename, checksums, bounded I/O, post-commit recall deletion, startup reconcile, quarantine, and process-service parity are implemented. Canonical record ownership and the documented quarantine/operator workflow remain broader structural work. |
| Strong provenance graph | Implemented storage-safety baseline (`CTX-05`) | `DerivedFrom`, `EvidenceFor`, `VerifiedBy`, `ArtifactOf`, and `Continuation` are strong edges; `SharesEntities` remains weak. Storage GC roots non-deletable stored records and traverses only strong edges, with deterministic and random-graph tests. Provenance admission/authority policy remains incomplete. |
| Incremental GC work | Partial | `ContextCatalog` applies a dirty-id upsert on ingest/maintain/tag instead of rebuilding every event; heap `replace_all` / restore still rebuild. Minor residency and Cold→External aging walk a `gc_work_batch` (default 4096) cursor so a heap at or below the batch keeps the previous full stable-order pass. |
| Immediate tool signal | Implemented (`CTX-08`) | Tool commit emits a bounded, body-free `WorkingSetSignal`; discovered entities heat related context before the immediately following model round while the tool body remains in `TurnFrame` until finalization. |
| Final model-consumption acknowledgement | Implemented (`CTX-07`) | `materialize` returns a non-consuming preview. After final packing, only a successful non-stale ModelOutput commits a bounded `ContextConsumptionAck` with the exact inline/external ids; failure paths do not reinforce. Fit-before-top-K, bounded/charged external refs, and bounded candidate generation are covered; workload cost evaluation remains separate. |
| Prompt authority separation | Implemented (`CORE-05`) | `PromptAssembler` keeps policy in System, renders selected history/external refs as delimited low-authority User observations, and preserves live file/tool output as Tool-role content. Injection regressions cover all three paths. |
| Real evaluation | Partial | EVAL-02 Context Bench (`agent-eval --context-bench`) is the current M15 decision instrument; 300×3 parked. Unit/property coverage is strong and the 10,000-turn residency regression exists. `agent-eval --compare-live` is the live paired coding harness (real model, independent workspaces, hidden verify). EVAL-01.1 writes per-cell bundles; EVAL-01.1b persists replayable file-content hidden asserts. EVAL-01.2 freezes the clustered C−A estimator; EVAL-01.3 re-freezes the gate at 300×3 / −5 pp (historical 30×3 is underpowered). EVAL-01.4e freezes the 509-task pack; EVAL-01.3b sets `SUITE_FROZEN=true` and declares retrieval secondaries in SPEC (no gate n/margin change). EVAL-01.3c locks the exact 300 acceptance ids and makes token diagnostics honor `cost_eligible`. EVAL-01.5 freezes the 30-task calibration sample; a file-only 9×3 live spend is in `crates/agent-eval/evidence/pilot-30` (`decision=pilot`). EVAL-01.5.p1 splits send vs pack and raises the shared live round cap to 48; remaining P0 SWE-bench (24k/12) is skipped as a floor-effect host. EVAL-01.5.p1b lands the shared model-backed bounded compactor for live B and C `TaskCompleted` distillation (CI keeps the scripted digest). EVAL-01.5.p1c is the retrieval-trust slice (catalog-wide search/inspect, trusted packed-set prompt); extra C rounds are still a treatment effect. P1 n=1 file-only + recall is collected (`rehydration-diag`); leftover extra rounds are mixed, not gone. Compaction cell harvest now sums `ContextMaintained` pass costs. P1 SWE-bench n=1 (`p1-swebench-diag`, pre-path-stamp): C 3/3 pass, A 0/3 at 48-round cap, B mixed. P1 after-path n=1: js-ms-negative C extra rounds gone this cell; recall extra rounds remain. Do not mix P0/P1 ITT tables. P1 file-only 9×3 on the current binary (`target/eval-evidence/p1-file-only-calibrate`, not `pilot-30`): ITT A=B=C=0.889, `decision=pilot`, analyze ineligible n=9 LCL=0 `degenerate=true`; `uuid-parity-keys` 0/9 hidden `cargo test`. Frozen SWE-bench 21×3 is still missing (after-proxy is n=1). The 300×3 non-inferiority run is still open. |

This table is the baseline for the work queue below. A checked defect in
`AUDIT_TODO.md` must not be reopened here under a new name.

### Re-grounding status

The original correctness list in this section is now closed in
`AUDIT_TODO.md`: episode-counter reset, cross-residency aging/dependencies/
scope close, tool-scope audit propagation, crash-safe store ownership,
operation serialization, logical-catalog diagnostics, and replay isolation all
have regression coverage. Do not reopen them here under stale baseline text.

The remaining context-runtime gaps are structural or evaluative:

- sourced `EpisodeOutcome` and atomic episode close/root release;
- bounded incremental minor-GC work and an explicit Storage-GC schedule;
- uniform immutable evidence/provenance beyond the final-response special path;
- real long-task A/B/C quality, cost, latency, and recovery evaluation.

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

The TaskAnchor is the minimum authoritative state that prevents goal drift.
Typed root claims now project into independent prompt/residency/storage
roots with `anchor_revision + source_field + RootReason`; Active/Suspended
downgrade and ResumePoint rehydration remain open.
It lives with the actor-owned `TaskManager`, not as an ordinary Pinned/Durable
`ContextItem`: otherwise a replaceable context policy could collect or rewrite
task authority, and task state would again be duplicated across orchestrators.

#### Current implementation boundary

The first `TaskToolRequirementSet` slice has since grown into a bounded,
actor-owned `TaskAnchor`; the two remain separate CAS surfaces because task
authority and tool-surface demand have different consumers and revision
cadences.

| Status | Scope |
| --- | --- |
| Implemented | Each `TaskRecord` owns a bounded `TaskAnchor`: original goal, current interpretation, constraints, acceptance criteria, plan progress, open loops, and typed working/evidence root claims. Whole-anchor and field-level CAS reject stale revisions; autonomous fields land directly, while goal/constraint patches use the approval boundary. `TaskAnchorChanged` is bounded and RuntimeCheckpoint v4 persists and validates anchors. |
| Implemented | `TaskToolRequirementSet` retains exact-name `MustSurface`/`PreferSurface`/`KeepReady` demand. `RoundSurfacePlan` is the bounded per-round projection; loading remains lifecycle, never activation or permission. |
| Implemented | Live restore rebases focus/surface/requirement revisions, applies capability state fail-closed, and publishes a bounded durable `RuntimeRestored` event before clearing the recovery fence. |
| Implemented | Task completion atomically closes context/task authority and commits exactly one immutable typed `CompletionRecord`. With an artifact workspace, the complete final assistant response is persisted before bounded context ingest and its ref is attached to the record. |
| Still open | `CTX-11`: Active/Suspended root downgrade plus a bounded, sourced `ResumePoint`/`TaskProgressView`; sourced `EpisodeOutcome`; ack/obligation-driven episode root release; richer provenance and outcome fields. `TaskAnchor` remains the only task-authority owner. |

Tool-demand semantics remain intentionally narrow:

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
one required tool may mark sibling schemas loaded. Per-tool capability
lifecycle and process/schema separation remain explicit follow-up work.

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

The actor now supplies a bounded `TaskAnchorView` plus typed root claims on
every materialize (`ContextHints.task` / `anchor_roots`). The engine copies
the view through without owning or scoring it, so model input is still built
through `ContextEngine::materialize` without making the engine a second task
manager. A versioned `ContextTaskView` wrapper is not required: the two
projections already travel together on `ContextHints`.

The next context integration step is Active/Suspended root downgrade and
ResumePoint rehydration. Its causal evaluation is queued behind the measured
tool-quality preflight (`TOOL-ENV-01`, `TOOL-EDIT-01`, `TOOL-VIEW-01`,
`TOOL-ERROR-01`): current failed-tool loops cannot prove that missing progress
state caused C's extra rounds. The contract may be designed and unit-tested in
parallel, but another live Context Bench comparison must first rerun the same
frozen cells on the corrected tool baseline with scoring unchanged. Target
task-state semantics (the current runtime exposes
`Active | Suspended | Completed` and has not yet landed the full root view):

- **Active:** the anchor is a mandatory materialization tier; current
  `working_refs` are online residency roots.
- **Suspended:** the anchor remains in RuntimeCheckpoint but is absent from an
  unrelated task's prompt; its refs are storage-retention roots and their
  bodies may cool/externalize. A bounded sourced `ResumePoint` captures the
  operational progress needed to continue without reconstructing it from
  dialogue. It is an actor-owned subrecord bound to `task_id + anchor_revision`,
  not a second task-authority store: the current objective is a projection of
  the anchor, and the resume record cannot rewrite the goal, hard constraints,
  or acceptance criteria. Its bounded `TaskProgressView` contains:
  - current objective, unresolved constraints/blockers and next actions;
  - checked file/entity refs with the observed content digest or revision and
    last-checked turn, never copied file bodies;
  - recent verification facts with a bounded command display/digest, target,
    outcome and evidence ref, never full stdout/stderr;
  - known failed-command facts with failure class, last result/evidence ref and
    whether the failure still blocks progress;
  - bounded working/evidence refs needed to rematerialize the next step.
  Every collection and string gets a named hard cap; overflow content is stored
  once as an artifact/context body and represented only by a typed ref. Runtime
  updates this record only from trusted safe-point facts (successful tool
  completion, verification outcome, durable turn commit, explicit suspend),
  with revision/CAS checks; model prose may propose progress but is not itself
  authority. Resume rematerializes only Anchor + ResumePoint refs, not the old
  transcript.
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

The first authoritative completion slice is implemented:

- every completed task owns exactly one immutable `CompletionRecord`, committed
  with the status flip and persisted in RuntimeCheckpoint v4;
- the record binds task id, frozen anchor revision, bounded summary,
  deterministic summary ref/digest, timestamp, and bounded artifact refs;
- when the composition root provides an artifact workspace, the complete final
  assistant response is written before bounded `ContextItem` ingest and its ref
  is attached to the record;
- the context close/maintenance transition succeeds before TaskManager commit;
  the bounded `TaskCompleted` event and post-completion GC make the outcome
  auditable without keeping completed task state Resident.

This is not yet the richer outcome contract below. The current record has no
typed success/partial/failure outcome, per-criterion results, verification or
unresolved-loop fields, episode-outcome refs, or explicit prepared/pending
state. Its `final_output_ref`/digest identify the bounded completion summary;
the full final response currently travels as an attached artifact ref, and
bare compositions without an artifact workspace skip that special retention
path.

Target complete shape:

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

Task lifecycle (`Completed` today; a possible richer `Closed` state later) and
outcome (`Succeeded`, `Partial`, etc.) must remain separate. Closing a task
must not falsely claim success.

The implemented context-first/task-commit ordering is the baseline. The target
adds explicit outcome validation and retryable preparation around it:

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

Target order (preserving the runtime's context-first/task-commit transaction):

1. Finish and durably identify the final committed model turn.
2. Persist the exact final output once, before context truncation, with digest.
3. Freeze the TaskAnchor revision and build/validate a bounded prepared
   CompletionRecord against acceptance evidence.
4. Transactionally close Episode/Task context scopes and transfer roots;
   rollback this plane if the transition fails.
5. Commit CompletionRecord + task status/terminal anchor patch, then publish
   the durable completion event. A post-commit audit failure fences recovery.
6. Run context GC; keep the outcome/output/evidence protected from Storage GC
   through strong typed edges.

Any pre-commit failure must leave a recoverable active/prepared task with its
old roots. Once context and TaskManager authority are aligned and committed, a
durable-event failure must fence mutation rather than roll one plane back by
itself. Future explicit `CompletionPrepared`/`ClosePending` metadata should
make retries idempotent; a queued journal write without the chosen durable
barrier is not sufficient reason to claim an auditable completion.

After commit, the task-owned CompletionRecord is not an online context root for
unrelated work. Its bounded summary remains searchable by task id, while a
future canonical evidence/catalog layer must make final output, deliverables,
verification, and their strong retention edges uniformly discoverable. A
follow-up should explicitly fetch/adopt the outcome or open a linked
continuation task.

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

The current code implements the trigger set and the immediate, body-free
`WorkingSetSignal` at tool commit. The important remaining delta is bounded
per-event work rather than repeatedly scanning the retained working set.

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
  **Done 2026-08-12.** An empty search result distinguishes the two
  cases: no filter reports that no catalog items match; a filter reports
  that nothing matches within the requested filter. The copy names the
  miss, it does not lecture about the working set.
  **Follow-up 2026-08-15.** Search is catalog-wide (Resident/Warm
  projections plus Stored). A live file is a hit, not an empty miss.
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
- [x] Make labels a real retrieval dimension. `ContextSearchQuery::label`
  filters `ExternalizedContext::tags` through the catalog's label index
  (exact, case-insensitive). Free-text still matches label keys as well as
  entities. Landed with the catalog/search-index co-design (2026-08-14).
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

- [x] **CTX-DISC-01** Define the bounded, versioned resource-ref/descriptor
  contract and distinguish `not found` from `provider unavailable`, stale
  revision, denied, and exact evidence absence.
  **Landed 2026-08-14.** `ResourceRef` / `ResourceDescriptor` /
  `DiscoveryMiss` live in `agent-contracts` (`resource://v1/<kind>/<id>[@rev]`).
  Inspect/fetch misses classify `not_found` vs `evidence_absent` (catalog
  still shows a terminal semantic) vs `provider_unavailable`. `stale_revision`
  and `denied` exist on the enum; inspect-by-id does not take a revision yet
  and no deny path is wired.
- [x] **CTX-DISC-02** Prototype non-vector federated search over the existing
  context and capability/tool providers; keep provider-owned indexes and
  deterministic ranking before adding artifacts/tasks/agents.
  **Landed 2026-08-14.** Internal `federate` merges Context hits then Tool
  hits (deterministic, capped). Public surfaces stay `context.manage` /
  `capability.manage`; there is no `runtime.search` schema. Capability search
  uses a provider-owned token index over name/description/owner/state/risk
  (case-insensitive; residual scan if no token hits). Artifact/Task/Agent/
  Skill/Event providers are still out.
- [x] **CTX-DISC-03** Enforce `search -> inspect/resolve -> explicit
  admit/surface/invoke`; record every transition and cap query count, fanout,
  rows, bytes, tokens, latency, and repeated-search loops.
  **Landed 2026-08-14 (Context + Tool prototype caps).** Search is read-only:
  it does not admit context or load a tool. `capability.manage` search/inspect
  are `TransientNoPersist` (load/unload still persist). Actor-owned
  `DiscoveryTurnBudget` caps searches per user turn (8) and identical
  fingerprints (2). Fanout 2, 32 rows, 4000 result chars, 256 query chars.
  Search latency is metered on the M15 event stream (`RunMetrics.search_ms_*`).
- [x] **CTX-EVENT-01** Generalize the current user-message path into a typed
  input envelope plus source-authorized state proposals while preserving the
  current direct, deterministic cancellation and command paths.
  **Landed 2026-08-14 (dialogue envelope).** `RuntimeInputEnvelope` carries
  source/authority/kind/lifecycle/task/turn/causal parent. `UserMessage`
  is Dialogue + UserSteering; `validate` refuses tool/collaborator
  UserSteering. `/focus` `/done` `/cancel` stay `RuntimeCommand`s and do
  not emit `UserMessageAccepted`. Dialogue `proposal` is always `None`
  (no NL-inferred authority patch). Residuals: no `submit_input` API;
  CancelTurn/Command kinds exist on the enum but are not constructed.
- [x] **CTX-EVENT-02** Give input records an explicit event lifecycle:
  `Received -> Interpreted -> Applied/Queued/Rejected -> Consumed ->
  Archived`; interruption and supersession must be revision-fenced and
  replayable.
  **Landed 2026-08-14 with residuals.** Successful ingest publishes
  `Applied`. One in-memory queue slot (`USER_INPUT_QUEUE_CAP = 1`) records
  `Queued` then applies after the busy turn ends (cancel or commit);
  overflow and cleanup still `Rejected`. `/cancel` stays a
  `RuntimeCommand` and, after the durable `TurnCancelled` barrier, emits
  `InterruptCommitted` (`kind=CancelTurn`, `causal_parent` = interrupted
  Applied id). Model consumption ack publishes `Consumed`; the
  `TurnCompleted` durable barrier then publishes `Archived` (input-record
  terminal, not context GC; appended before the TurnCompleted flush). Residuals: queue is not checkpointed or
  crash-durable; lifecycle follow-ups are not themselves durability
  barriers; `Received`/`Interpreted` unused; no NL-inferred patch.
- [x] **CTX-EVENT-03** Replace the current full-content
  `UserMessageAccepted` audit payload with a bounded preview plus stable body
  ref, digest, size, authority, and task/turn ids. Store the exact body once
  in the evidence plane and budget its model projection separately; event
  logging must not become an unbounded duplicate of the transcript.
  **Landed 2026-08-14.** Preview cap 240 chars; old JSONL `content`
  deserializes as `preview`. With a workspace, owner `user-input` seals the
  exact bytes. Replay resolves `body_ref` from an optional workspace
  (`ReplayConfig.artifact_workspace` / `agent-replay --workspace`); a
  truncated preview without a workspace fail-closes.
- [x] **CTX-GC-10** Couple search/resolve signals to bounded access
  reinforcement and GC explanations, but never let a search hit override
  terminal semantic state or mandatory TaskAnchor roots. Landed 2026-08-12:
  `context.search` now stamps a bounded slice of hits (at most 8 per call)
  with a fresh recency clock and GC-epoch anchor, delaying Cold -> External
  aging for re-referenced entries; terminal hits stay filtered out by
  `externally_retrievable` and GC roots are untouched, so a search can
  reinforce access without ever resurrecting dead semantics or unmarking a
  mandatory root. Covered by `search_hits_stamp_a_bounded_recency_
  reinforcement` and `search_reinforcement_delays_cold_to_external_aging`.
- [x] **CTX-GC-11** Grade retrieval access signals instead of one flat
  reinforcement, and bound repeated-search gaming. Landed 2026-08-14:
  search-hit is the weakest signal (at most one Cold-aging delay until a
  stronger read, per-item cooldown inside one `event_seq`, and one
  identical-query stamp per user turn), inspect/fetch are stronger
  deliberate reads, `admit` remains an explicit residency action, and
  `ContextConsumptionAck` is the strongest online evidence (turn clocks +
  `access_count` + GC epoch). A weaker signal never overwrites a stronger
  one, so a loop of broad searches cannot pin never-used Cold entries.
  Covered by `search_hits_stamp_a_bounded_recency_reinforcement`,
  `search_reinforcement_delays_cold_to_external_aging`,
  `identical_search_query_budget_blocks_a_second_stamp_in_the_same_turn`,
  `search_hit_cools_down_inside_the_same_event_seq`,
  `inspect_outranks_search_and_resets_saturation`,
  `search_saturation_cannot_pin_cold_entries_across_gc_passes`, and
  `consumption_ack_stamps_an_external_descriptor_without_reactivating_it`.

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

### Current slice (registered 2026-08-14 evening) — close the M15 evidence loop

The GC↔Search machinery and its instrumentation landed today; the live
paired cells produced the first real treatment-effect signal: dynamic
matches append on hidden-check success but spends more end-to-end
because it takes extra tool rounds (deferred first fix, repeated
probes/rereads — see EXPERIMENTS "Why C spent more"). The evaluation
loop, not more machinery, is now the bottleneck.

Decision (2026-08-14): Search/GC evaluation is folded directly into
M15. Retrieval metrics — search recall/latency, found-after-forgotten,
graded-access distribution — ride the same live A/B/C cells and
evidence bundles as secondary lifecycle endpoints of the same runs;
there is no separate, later Search/GC experiment. Declared in SPEC at
EVAL-01.3b (no gate change). Discovery feature residuals (Artifact/Task/Agent/
Skill/Event providers) stay out of scope; M15 measures the retrieval
surface that exists.
**2026-08-15.** `--analyze-evidence` now prints those secondaries from
the same cost-eligible A/C pairs (search calls/hits/empty/p50,
found-after-forgotten, graded-access stamps). They stay out of the LCL
gate. Old bundles missing latency/access keys read as 0. SPEC n/margin
unchanged. Phase 2 Active/Suspended root downgrade remains outside this
slice.

Working order (owning entries stay authoritative: `EVAL-02` then `EVAL-01`
in AUDIT; this list does not duplicate checkbox state):

0. **EVAL-02 Context Benchmark is the current M15 decision instrument.**
   12 tasks in `crates/agent-eval/context-bench/` ask where dynamic
   context helps or hurts a coding agent (horizon, long refactor,
   semantic recall, supersession, task switch, noise). Wave 1 is 12×A/C
   plus rolling only on `horizon_long` / `semantic_recall` /
   `task_switch` (27 cells at repeats=1). Pack/SPEC are hash-frozen;
   wave-1 live is under `evidence/context-bench-wave1/`. Before another
   live wave, finish the tool-quality preflight (`TOOL-ENV-01` →
   `TOOL-ERROR-01`; owning specs in TOOL_ECOSYSTEM / AUDIT). `CTX-11` is
   queued behind that cleaner baseline. Do not continue the 30-task
   pilot or mix P0/P1 ITT tables. analysis.v2 stays frozen. Do not close
   M15.

1. Freeze the 300-task suite (`EVAL-01` closure item 2). Heterogeneous
   real coding tasks with executable hidden verification; treat the
   suite as its own reviewed deliverable (language, size, edit shape and
   multi-turn recall pressure reviewed before freezing); no one-line
   stand-ins. `SUITE_FROZEN=true` only after that review, and no
   acceptance cells before it. Source from real repository histories
   where practical. **Done EVAL-01.3b:** pack frozen at 509/300;
   `SUITE_FROZEN=true`; SPEC re-registered with retrieval secondaries
   (search recall/latency, found-after-forgotten, graded-access
   distribution) and no gate n/margin change. **Done EVAL-01.3c:** exact
   300 acceptance ids locked (`agent-eval.acceptance.v1`, sha256
   `7ff6b5dd…`, harvest sizes 107/147/46 because SWE-bench large n=46);
   gate is `n_tasks==300 AND evidence_ids==acceptance_ids`, not any
   ≥300 subset of the 509 pack. Cost diagnostics omit
   `cost_eligible=false` cells; cost-missing rate is reported separately.
   Do not collect 300×3 acceptance cells until remaining calibration.
2. Calibrate before spending the 300×3 budget: one frozen
   non-acceptance pilot (~30 tasks × 3) to check the power simulation's
   variance/clustering assumptions against real cells. Amend n only by
   re-registration (an EVAL-01.3 amendment), never after seeing
   acceptance cells.
   **Partial EVAL-01.5 (2026-08-14):** sample frozen at 30 ids
   (`fa8c5308…`, 10/10/10 size, 9 file + 21 SWE-bench). `--pilot` lists
   it; `--pilot-run` is live A/B/C with executable hidden commands
   (default file-only); `--include-swebench` clones `base_commit` and
   scores a git diff with the official Docker harness;
   `--pilot-calibrate` prints `decision=pilot` and cannot open the
   300×3 gate.
   **Live file-only spend (2026-08-14):** 9 tasks × 3 repeats × 3
   engines = 81 cells under `crates/agent-eval/evidence/pilot-30`. ITT A=C=
   0.778, B=0.704; task-level corr(A,C)=0.78; diagnostic C−A LCL=
   −0.146 (not a gate). `analyze()` stays ineligible (`n_tasks=9 != 300`
   and evidence ids ≠ frozen acceptance set). Pooled φ(A,C)=0.36 is
   labeled confounded; task-residual corr(A,C)=−0.41 is the power-model
   diagnostic (n=9, noisy). Cost-missing 6/27; cost-eligible paired
   tokens n=21 (A 54318 / C 56251).
   **2026-08-15.** `--pilot-calibrate --file-only` keeps that 9-task
   table when the same directory also holds P0 SWE-bench floor cells.
   Retrieval secondaries on those 21 cost-eligible A/C pairs: search
   calls 0.2/0.2; C forgotten/recovered 11.3/0.9 vs A 0/0; not in the
   LCL gate. Access-stamp keys are absent on those older summaries
   (read as 0). A whole-directory calibrate without `--file-only`
   mixes P0 SWE-bench zeros into ITT — do not use that table. **EVAL-01.5.p1:** remaining P0
   SWE-bench under send=kernel-fallback 19904 / rounds=12 is skipped
   (floor effect: empty context still overflowed; 12 rounds aborted
   tool loops). Do not mix P0 and P1 ITT tables. P1 shared host:
   declared send window (default 128k), kernel pack 24k for C/B, A
   grows until send, 48 rounds shared. Do not amend n. Do not retune
   scoring. **Done EVAL-01.5.p1b:** model-backed bounded compaction B
   (shared operator with C `TaskCompleted` distillation). **Partial
   EVAL-01.5.p1c:** catalog-wide search/inspect; system prompt is a short
   runtime contract (selected frame ≠ full catalog; context/capability
   tools); assembler headers stay labels/facts (`path=`, catalog census),
   not retrieval tutorials. Live `fs.read` stamps `metadata.path` /
   `revision` onto `ContextItem` so latest-file-body and path search work
   without parsing numbered lines. Smoke
   fixtures stay file-content; executable hidden stays on the suite
   pack. **P1 n=1 rehydration diag (2026-08-15):** 9 file-only +
   `recall_after_fix` collected in `target/eval-evidence/rehydration-diag`
   (not the P0 81-cell table). pep616 C extra rounds/tools converged;
   js-ms-minutes C extra tools gone; recall extra rounds remain (21r/25t
   vs A 15r/12t). Mixed leftover: js-ms-negative C 14r vs A 5r; rust-jcs
   C fewer (4r vs A 8r). Empty-assistant flake (`usage_incomplete`, 0
   tokens): js-ms-minutes B, openai-wire B, rust-grep C — not a compact
   crash, not C vs A. Catalog search almost unused. Cell `compact=0/0`
   was a harvest bug (later GC snapshot wiped B fold); metrics now sum
   `ContextMaintained` per-pass costs. **P1 SWE-bench n=1 (2026-08-15).**
   Three Django tasks in `target/eval-evidence/p1-swebench-diag` (not
   `pilot-30`; pre-path-stamp binary). C passed 3/3 (11749 25r/38t
   `model_in=420258`; 11999 27r/43t `520717`; 12708 24r/55t `679564`).
   A hit the shared 48-round cap on all three. B: 11749 empty-assistant
   (`usage_incomplete`, 0 tokens); 11999 1r/0t then docker eval fail;
   12708 passed 21r/42t. C catalog search stayed 0; forgotten/reread
   remained. Do not mix with the P0 81-cell table. Do not amend n.
   **P1 after-path n=1 (2026-08-15).** `target/eval-evidence/p1-after-path`
   on the path/contract binary. `js-ms-negative-parse`: C extra rounds
   gone (14r/22t → 9r/17t vs A 11r/18t; C `search=1/2`).
   `recall_after_fix` via `--compare-live` (not `--pilot-id`): C still
   21r/30t with catalog search 0, recovered 9/26, reread 7, `git.status`
   5/5 failed; A verify-failed 16r (scratch notes). Extra rounds remain
   a treatment effect on recall-class. Do not retune scoring. Do not
   amend n. Do not mix ITT tables.
   **P1 after-anchor n=1 (2026-08-15).** `target/eval-evidence/p1-after-anchor`
   on the TaskAnchorView + path/contract binary. `recall_after_fix`: A/B/C
   all verify-failed on `scratch.md` `4B` (C also `visit_all`); C 16r/21t
   search 0, recovered 5/19, reread 5, `git.status` 4/4 failed vs A 13r/10t.
   `js-ms-negative-parse`: C 10r/14t pass vs A 11r/19t pass, C search 0;
   B 3r verify-failed (`node --test`). `--analyze-evidence` on the dir:
   ineligible n=2; retrieval secondaries search calls 0/0; C
   found-after-forgotten forgotten=15.5 recovered=2.5 (A 0). Extra rounds
   on recall-class remain. Do not mix ITT tables. Do not amend n.
   **P1 SWE-bench after-anchor n=1 attempt (2026-08-15).**
   `target/eval-evidence/p1-swebench-after-anchor` `django-13344`: A/B/C
   turn-1 HTTP 403 (`permission_error`: Access from this region requires
   trusted account access); cells kept as ITT `outcome=error` and
   cost-ineligible. A cheap `fix_off_by_one` probe (`p1-provider-probe`)
   hit the same 403. Not a coding result. **Cause (2026-08-15):**
   workspace `reqwest` had `default-features = false` without
   `system-proxy`, so live eval ignored Windows Internet Settings
   (`127.0.0.1:7897`) and went direct; curl via that mixed port is 200.
   Do not mix those 403 cells into an ITT table.
   **P1 SWE-bench after-proxy n=1 (2026-08-15).** `system-proxy` binary;
   `target/eval-evidence/p1-swebench-after-proxy` (not the 403 dir).
   Cheap `fix_off_by_one` probe (`p1-provider-probe-proxy`) A/B/C all
   passed. `django-13344` gold harness ran (`resolved=0` / `unresolved=1`
   on every arm): A/B/C `verify_failed`, usage complete, not 403.
   C 33r/59t `model_in=926950` search 2/3 forgotten 48 recovered 0;
   B 35r/82t `1174170` compact 14445/814; A 47r/69t `1550204`.
   `--analyze-evidence`: ineligible n=1; cost-eligible C−A tokens
   −623254, rounds 47/33; retrieval search A/C 0/2. `git.status` ok
   (SWE-bench clone is a git repo). Do not mix with P0, `p1-swebench-diag`,
   or the 403 cells. Do not amend n. Do not retune scoring.
   **P1 SWE-bench after-proxy n=1 `django-13809` (2026-08-15).** Same dir.
   B gold passed (43r/64t `model_in=1214260`). A and C hit the shared
   48-round cap (`outcome=error`, usage complete); C search 0,
   lifecycle=0. `--analyze-evidence` on 13344+13809: ineligible n=2;
   ITT A=C=0; cost-eligible tokens A 1573556 / C 1076972; search A/C
   0/1. Do not mix ITT tables. Do not amend n.
   **P1 after-proxy `django-14007` (2026-08-15).** Same dir. A 48-round
   cap; B/C gold `verify_failed` (C 44r/72t `1271989` search 0 forgotten
   63 recovered 0). `--analyze-evidence` n=3 ineligible; ITT A=C=0;
   cost-eligible tokens A 1477619.7 / C 1141977.3. Do not mix ITT tables.
   Do not amend n.
   **P1 after-proxy `django-14011` (2026-08-15).** Same dir. A/B/C gold
   `verify_failed` (C 27r/45t `681903` search 0 forgotten 37; A 29r/39t
   `594658`; B 35r/65t `1288970`). `--analyze-evidence` n=4 ineligible;
   ITT A=C=0; cost-eligible tokens A 1256879.2 / C 1026958.8. Do not mix
   ITT tables. Do not amend n.
   **P1 after-proxy `django-15268` (2026-08-15).** Same dir. B gold passed
   (46r/61t `1212441`). C 48-round cap (`outcome=error`, lifecycle=0).
   A mid-run HTTP 401 `INVALID_API_KEY` (`usage_incomplete`, cost-missing).
   `--analyze-evidence` n=5 ineligible; ITT A=C=0; cost-missing 1/5;
   cost-eligible still n=4 (15268 A dropped). Do not mix ITT tables.
   Operator: 401 is relay jitter, not a dead key. Continue frozen ids
   in the same dir.
   **P1 after-proxy `django-15503` (2026-08-15).** Same dir. A gold
   `verify_failed` 20r/32t `493440`. B/C 48-round cap (`outcome=error`;
   C search 1/0 empty 1, forgotten 0 recovered 0). `--analyze-evidence`
   n=6 ineligible; ITT A=C=0; cost-missing 1/6; cost-eligible tokens A
   1104191.4 / C 1062320.8. Do not mix ITT tables. Do not amend n.
   **P1 after-proxy `django-15695` (2026-08-15).** Same dir. A/B/C gold
   `verify_failed` (A 38r/78t `1666651`; B 42r `1655407` search 1/1;
   C 44r `1503592` search 1/1 forgotten 55 recovered 0).
   `--analyze-evidence` n=7 ineligible; ITT A=C=0; cost-missing 1/7;
   cost-eligible tokens A 1197934.7 / C 1135866.0. Do not mix ITT
   tables. Do not amend n.
   **P1 after-proxy `django-16642` (2026-08-15).** Same dir. A/B/C gold
   `verify_failed` (A 20r `526493`; B 15r `224632` search 1/2; C 8r
   `77182` search 1 empty forgotten 14 recovered 0). `--analyze-evidence`
   n=8 ineligible; ITT A=C=0; cost-missing 1/8; cost-eligible tokens A
   1102014.4 / C 984625.4. Django ids in this dir are done. Do not mix
   ITT tables. Do not amend n.
   **P1 after-proxy `matplotlib-23314` (2026-08-15).** Same dir. A/B/C
   gold passed (A 32r `773272` search 1/2; B 39r `732591`; C 35r
   `751527` search 2/14 forgotten 43 recovered 0). First both-pass pair
   in this dir. `--analyze-evidence` n=9 ineligible; ITT A=C=0 still
   degenerate (this task A=C=1); cost-missing 1/9; cost-eligible tokens
   A 1060921.6 / C 955488.1; both-pass n=1 C-A=-21745. Do not mix ITT
   tables. Do not amend n.
   **P1 after-proxy `pylint-4551` (2026-08-15).** Same dir. A/B 48-round
   cap on turn 4 (`outcome=error`; A 101r `4372763`). C gold
   `verify_failed` 63r `1597882` forgotten 99 **recovered 12** (first
   non-zero recover in this dir; catalog search 0, access ack 100).
   `--analyze-evidence` n=10 ineligible; ITT A=C=0; cost-missing 1/10;
   cost-eligible tokens A 1428904.0 / C 1026865.2; recovered C mean 1.3.
   Do not mix ITT tables. Do not amend n.
   **P1 after-proxy `pytest-5787` (2026-08-15).** Same dir. A/B/C gold
   `verify_failed` (A 32r `1146981`; B 25r `794492`; C 31r `863738`
   search 1/2 forgotten 30 recovered 0). `--analyze-evidence` n=11
   ineligible; ITT A=C=0; cost-missing 1/11; cost-eligible tokens A
   1400711.7 / C 1010552.5. Next `pytest-6202` n=1. Do not mix ITT
   tables. Do not amend n.
   **P1 after-proxy `pytest-6202` (2026-08-15).** Same dir. A/B gold
   passed (A 30r `595044`; B 24r `459627`). C 48-round cap (`outcome=error`,
   lifecycle=0, forgotten=0). First non-zero ITT pair in this dir:
   A=1 C=0 d=-1. `--analyze-evidence` n=12 ineligible; primary C-A
   mean=-0.083 LCL=-0.233 `degenerate=false`; cost-missing 1/12;
   cost-eligible tokens A 1327469.2 / C 1018846.4. Not a gate. Do not mix
   ITT tables. Do not amend n.
   **P1 after-proxy `pytest-7571` (2026-08-15).** Same dir. A/B/C gold
   passed (A 29r `596468`; B 32r `740270`; C 38r `756640` forgotten 47
   recovered 0). Second both-pass pair. `--analyze-evidence` n=13
   ineligible; ITT mean=-0.077 LCL=-0.214 `degenerate=false`;
   cost-missing 1/13; cost-eligible tokens A 1266552.4 / C 996995.8;
   both-pass n=2 C-A=+69213.5. Do not mix ITT tables. Do not amend n.
   **P1 after-proxy `scikit-learn-11310` (2026-08-15).** Same dir. A
   48-round cap (`2064105`). B gold passed 29r. C gold passed 48r
   `1328911` forgotten 66 recovered 0 (search 0). First A=0 C=1 pair
   (d=+1). `--analyze-evidence` n=14 ineligible; ITT mean=0 LCL=-0.186
   `degenerate=false`; cost-missing 1/14; cost-eligible tokens A
   1327902.6 / C 1022527.8. Not a gate. Do not mix ITT tables. Do not
   amend n.
   **P1 after-proxy `scikit-learn-13496` (2026-08-15).** Same dir. A gold
   passed 25r `691931`. B/C 48-round cap (C search 1 empty, forgotten 0).
   Second A=1 C=0 pair (d=-1). `--analyze-evidence` n=15 ineligible; ITT
   mean=-0.067 LCL=-0.275 `degenerate=false`; cost-missing 1/15;
   cost-eligible tokens A 1282476.1 / C 1059177.3. Not a gate. Do not mix
   ITT tables. Do not amend n.
   **P1 after-proxy `scikit-learn-14894` (2026-08-15).** Same dir. A gold
   `verify_failed` 7r/7t `28616` (4 turns). B gold passed 53r. C 48-round
   cap forgotten 0. `--analyze-evidence` n=16 ineligible; ITT mean=-0.062
   LCL=-0.256 `degenerate=false`; cost-missing 1/16; cost-eligible tokens
   A 1198885.4 / C 1055563.8. Sklearn ids in this dir are done. Do not
   mix ITT tables. Do not amend n.
   **P1 after-proxy `sphinx-8548` (2026-08-15).** Same dir. B gold
   `verify_failed` 32r. C 2r `verify_failed` forgotten 3 recovered 0.
   A HTTP 502 `upstream_error` (`usage_incomplete`). `--analyze-evidence`
   n=17 ineligible; ITT mean=-0.059 LCL=-0.240; cost-missing 2/17
   (15268 401 + this 502). Treat 502 as relay jitter; continue.
   Do not mix ITT tables. Do not amend n.
   **P1 after-proxy `sympy-22914` (2026-08-15).** Same dir. B/C HTTP 429
   `DAILY_LIMIT_EXCEEDED` (`model_in=0`, `usage_incomplete`). A HTTP 502
   (`usage_incomplete`). `--analyze-evidence` n=18 ineligible; ITT
   mean=-0.056 LCL=-0.226; cost-missing 3/18. This is a quota stop, not
   jitter. Do not start more live cells until the relay daily limit
   resets. Do not mix ITT tables. Do not amend n.
   **Retry after quota (2026-08-15).** Operator: continue. Moved the
   429/502 sympy r1 out of the ITT dir into
   `target/eval-evidence/p1-swebench-quota-429` so `--pilot-run` can
   rewrite r1 without mixing a second repeat. Same after-proxy dir.
   **P1 after-proxy `sympy-22914` retry (2026-08-16).** A/C gold passed
   (A 17r `278774`; C 11r `151690` forgotten 25 recovered 0). B
   `verify_failed` 9r. Third both-pass pair. `--analyze-evidence` n=18
   ineligible; ITT mean=-0.056 LCL=-0.226 `degenerate=false`;
   cost-missing 2/18; cost-eligible tokens A 1141378.4 / C 999071.7;
   both-pass n=3 C-A=+3781. Not a gate. Next: backfill `django-11749`
   n=1 into this dir (diag-only on the old binary). Do not mix ITT
   tables. Do not amend n.
   **P1 after-proxy `django-11749` (2026-08-16).** Same dir, not diag.
   A gold `verify_failed` 6r `46060`. B/C empty-assistant flake
   (`model_in=0`, `usage_incomplete`, 1r/0t). `--analyze-evidence` n=19
   ineligible; ITT mean=-0.053 LCL=-0.214; cost-missing 3/19. Do not mix
   ITT tables. Do not amend n.
   **P1 after-proxy `django-11999` (2026-08-16).** Same dir, not diag. C
   gold passed 32r `693668` search 2/2 forgotten 39 recovered 0. A gold
   `verify_failed` 2r `6009`. B fixture-turn timeout (`usage_incomplete`).
   Second A=0 C=1 pair (d=+1). `--analyze-evidence` n=20 ineligible; ITT
   mean=0 LCL=-0.177 `degenerate=false`; cost-missing 3/20; cost-eligible
   tokens A 1074592.0 / C 981106.8. Not a gate. Next `django-12708` n=1
   (last frozen SWE-bench missing from this dir). Do not mix ITT tables.
   Do not amend n.
   **P1 after-proxy `django-12708` (2026-08-16).** Same dir, not diag.
   A/B/C gold passed (A 29r `928395`; B 40r `1125479` search 1/2; C 18r
   `374119` forgotten 31 recovered 0 search 0). Fourth both-pass pair
   (d=+0). `--analyze-evidence` n=21 ineligible; ITT mean=0 LCL=-0.168
   `degenerate=false`; cost-missing 3/21; cost-eligible n=18 tokens A
   1066469.9 / C 947385.2; both-pass n=4 C-A=-135733. Discordant ITT
   still pytest-6202 / sklearn-13496 d=-1 and sklearn-11310 /
   django-11999 d=+1. Frozen SWE-bench n=1 in this dir is complete
   (21/21; still repeats=1, not ~30×3, not 300×3, not an M15 close).
   Next: working-order item 3 remaining (episode-rotation distillation),
   then extra-round re-measure — not 30×3. Do not mix ITT tables. Do not
   amend n.
   **P1 file-only 9×3 (2026-08-16).** Current binary (git-seed + episode
   distill) in `target/eval-evidence/p1-file-only-calibrate` (not
   `pilot-30`, not after-proxy). 9×3×3=81 cells. `--file-only
   --pilot-calibrate`: `decision=pilot` coverage 9/30 cells 81/270;
   ITT A=B=C=0.889; diagnostic C−A mean=0 LCL=0 `degenerate=true`
   (all task diffs 0; residual corr undefined). `--analyze-evidence`
   ineligible n=9; SPEC hash unchanged. 72 pass / 9 `verify_failed`;
   `uuid-parity-keys` 0/9 all arms (hidden `cargo test --offline`);
   other 8 tasks 9/9. Cost-missing 0/27; cost-eligible n=27 tokens A
   62899.6 / C 69826.4 C−A=+6926.8 rounds 8.9/9.5. Retrieval: search
   0.1/0.0; C forgotten/recovered 14.7/1.4 vs A 0/0; C access ack 12.4.
   Not a gate. Not mixed with P0 81-cell or after-proxy n=1. Do not
   retune scoring. Do not amend n. Remaining item 2: frozen SWE-bench
   21×3 (after-proxy is n=1).
3. Model-backed bounded compaction B (`EVAL-01` closure item 5): B must
   summarize with a model under a budget, with its compactor cost
   counted; the scripted digest remains a deterministic CI arm only.
   Build the compactor once as a shared component: the same bounded
   model summarizer is the B arm *and* C's episode/trajectory
   distillation operator (Phase 3). Same summarizer in both arms keeps
   the comparison fair — a measured C−B difference is then purely
   lifecycle + retrieval, not summary quality. The two roles differ only
   in what happens to the source: B folds history lossily, while inside
   C a summary is a `derive`d item with `DerivedFrom` provenance and the
   raw bodies stay externally retrievable, so a bad summary is
   correctable through search/fetch/admit instead of permanent.
   **Partial 2026-08-15 (EVAL-01.5.p1b).** `BoundedCompactor` lives in
   `agent-contracts` (source/output char caps 2000/512). Live eval and
   TUI inject `ModelBackedCompactor` into rolling B and dynamic C; CI
   rolling uses `ScriptedCompactor` (0 provider tokens). B folds without
   holding the engine mutex across the model call; compact failure or
   empty output falls back to a bounded marker and does not fail the
   coding turn. C distills only on `TaskCompleted` (episode-rotation
   distillation still deferred): the result is a `derived` Summary with
   `DerivedFrom` edges; source items stay. Compactor usage is on
   `ContextDiagnostics` and `manager_token_cost`. Do not retune scoring.
   Do not amend SPEC n/margin.
   **Follow-up 2026-08-16.** Episode-rotation distillation uses the same
   operator: plan under the lock, compact after it drops, insert an
   `episode-derived` Durable Summary on the task scope with `DerivedFrom`
   edges. Raw episode bodies stay. At most one live episode card per task
   (a later rotation supersedes the previous card). Compact failure falls
   back to a bounded marker and does not fail the user turn. Without a
   compactor, rotation stays promote-and-evict. Do not retune scoring.
   Do not amend SPEC n/margin.
4. Explain and reduce dynamic's extra live rounds — the measured
   treatment effect. The lever is the just-landed retrieval surface
   (catalog indexes, graded access, discovery descriptors): make recall
   cheap and trusted so the model re-verifies less, then re-measure on
   `recall_after_fix`-class fixtures. Scoring stays frozen: behavior
   changes come from retrieval/navigation and from removing prompt
   distrust, not from adding tutorials. Any scoring change waits for
   suite evidence, not n=1 live cells.
   **Partial 2026-08-15 (EVAL-01.5.p1c).** 2026-08-14 traces stand: extra
   rounds were not "the model cannot see the current-turn tool output"
   (tool results render in the turn frame as `ModelRole::Tool`). Causes:
   prompt distrust (working set called a cache / optional), empty search
   of a still-Resident file (first smoke), a no-tool first turn on C,
   failed `git.status` / `shell.exec` probes, extra rereads. Across user
   turns, successful `fs.read`-shaped observations are ephemeral; the
   latest-file-body-of-active-task policy is the keep-in-view mechanism.
   This slice does not retune scoring and does not hide `git.status`.
   Changes: `DEFAULT_CODING_AGENT_SYSTEM_PROMPT` is a short runtime
   contract (selected frame is not the full catalog; context tools
   search/retrieve; capability tools discover/load), not a retrieval
   tutorial; assembler headers are labels/facts (`path=`, catalog census)
   only; `search_ids` / `search_catalog` /
   `inspect_external` cover Resident/Warm/Stored; hits carry `residency=`
   as data, not a next-step tutorial; `fetch_external` stays store-only
   and a Resident/Warm id states the body is already in the working set;
   empty search copy is a catalog miss, not "no externalized items".
   Extra live rounds remain a treatment effect to re-measure. Do not
   amend SPEC n/margin.
   **Re-measure 2026-08-15 (n=1, P1 host, not an ITT table).** Evidence
   in `target/eval-evidence/rehydration-diag`. pep616 C−A rounds/tools
   19/27 vs 12/11 → 15/20 vs 13/16 (the extreme inflation converged).
   js-ms C extra tools gone (20→12, both 9 rounds). `recall_after_fix`
   extra rounds did not disappear (17/21 → 21/25 vs A 15/12); leftover
   is failed `git.status` / `shell.exec` and extra rereads, and C made
   zero catalog searches. Empty-assistant flake (`usage_incomplete`, 0
   tokens): js-ms-minutes B, openai-wire B, rust-grep C — provider miss,
   not a compact crash. **File-only n=1 slice complete (2026-08-15).**
   Same dir, remaining 7 plus the first 3: C extra rounds persist on
   recall and js-ms-negative-parse; gone on openai-wire / itertools /
   symbols (C≈A); rust-jcs C used fewer rounds. uuid-parity still 0/3
   (same as P0). Compaction harvest now sums maintenance pass costs so
   a later zero GC snapshot cannot wipe B fold. Do not re-run file-only
   into the P0 81-cell table. **P1 SWE-bench n=1 (2026-08-15).** Evidence
   in `target/eval-evidence/p1-swebench-diag` (pre-path-stamp binary).
   C passed all three Django cells; A exhausted 48 rounds on all three;
   B mixed (11749 `usage_incomplete` 0-token; 11999 1r/0t verify fail;
   12708 pass). C search stayed 0. Not an ITT table.
   **P1 after-path n=1 (2026-08-15).** `p1-after-path`: js-ms-negative
   C extra rounds gone (9r vs A 11r, C search 1/2); recall C still 21r/30t
   search 0 (A verify-failed 16r). Do not amend n.
   **P1 after-anchor n=1 (2026-08-15).** `p1-after-anchor`: recall A/B/C
   verify-failed (`4B` notes); C 16r vs A 13r, search 0. js-ms-negative
   C 10r pass vs A 11r pass, search 0; B hidden fail. Do not amend n.
   **P1 after-episode-distill n=1 (2026-08-16).** Episode-rotation
   distill binary; `target/eval-evidence/p1-after-episode-distill` (not
   after-anchor / after-proxy ITT). `recall_after_fix`: B gold passed
   15r/14t; A 17r/15t `verify_failed` (scratch missing Breville/200);
   C 17r/22t `verify_failed` (scratch missing `4B`), catalog search 0,
   forgotten 18 recovered 5, `git.status` 2/2 failed, access ack 127.
   C rounds tied A this cell; extra tools remain. `js-ms-negative-parse`:
   A/B/C all passed (A 6r/8t; B 9r/10t; C 6r/8t forgotten 5 recovered 0
   search 0). Extra C rounds on js-ms stay gone. n=1 diagnostic; leftover
   on recall is still notes + failed `git.status`, not catalog search.
   Do not retune scoring. Do not amend n. Do not mix ITT tables.
   **Follow-up 2026-08-16.** File-only / smoke eval workspaces now
   `git init` + seed commit when `.git` is absent, so `git.status` is a
   real probe. SWE-bench clones already have `.git` and are left alone.
   Do not hide `git.status`. Do not retune scoring.
   **P1 file-only 9×3 (2026-08-16).** See item 2 (`p1-file-only-calibrate`).
   Extra C rounds on that table are small (9.5 vs A 8.9) with ITT tied;
   leftover n=1 recall cells stay a separate diagnostic. Do not mix ITT
   tables.
5. Executable hidden build/test verification for fixtures (`EVAL-01`
   closure item 5, first half).
   **Decision 2026-08-15.** Do not bind the five smoke `FIXTURES` to
   executable hidden commands. Those fixtures stay interpreter-free
   file-content asserts so CI does not fail-closed on a missing
   `python`/`cargo` and does not exec model-written files on the host.
   Executable hidden already lives on the suite pack (overlay +
   `hidden_commands`; SWE-bench docker remains opt-in). Dual-oracling
   smoke would make the cheap path as fragile as the suite path. Scoring
   of the five smoke fixtures is unchanged. Do not amend n.

Explicitly not in this slice: retuning GC scoring from small-n live
cells; Artifact/Task/Agent/Skill/Event discovery providers; a public
`runtime.search` schema; any vector index (AGENTS.md invariant 8);
Phase 2 incremental GC beyond what the catalog forces; and scale
engineering for thousands of tools or million-entry stores without a
measured need. The PLAT/M12/M13 trusted-execution lane continues
independently per ROADMAP and is not gated by this slice.

### Completed slice (2026-08-14) — pair GC with Search/Discovery

GC currently runs ahead of retrieval: eviction/externalization policy is
richer than the machinery for finding things back. This slice closes that
gap instead of adding more GC scoring rules. Every item below lives where
its owning entry already is (Phase/AUDIT/TOOL ids); this list only fixes
the working order and does not duplicate checkbox state:

1. [x] Fix `search.grep` cancellation (`TOOL-01`, AUDIT P1): search
   observes its `CancellationToken` between files and periodically inside
   large files, and a cancelled scan returns `ok: false` with
   `metadata.cancelled` plus any hits already found. Landed 2026-08-14.
2. [x] Land the `ContextCatalog` authority/body split *together with* its
   query indexes (the Phase 1 catalog item). Landed 2026-08-14: one
   `item_id -> location` directory whose id/task/scope/kind/entity/label/
   lifecycle indexes serve GC recall and `context.search`. Candidate
   generation uses index buckets plus the existing bounded ranking;
   `label` is a real `ContextSearchQuery` dimension. Authority metadata
   stays on the single body; the three stores remain the serde/body
   layout. A summary/uri needle that hits no entity/label key still
   residual-scans.
3. [x] `CTX-GC-11`: graded access signals (search-hit weakest, inspect/fetch
   stronger, `admit` an explicit residency action, consumption ack the
   strongest) plus per-item cooldown and repeated-search budgets. Landed
   2026-08-14: signal strength writes through the body (`AccessSignal` +
   `search_reinforce_count` on `ExternalizedContext`); search cannot pin
   Cold entries after one aging delay. Independent of item 4.
4. [x] `CTX-DISC-01..03` over Context + Tool only (Phase 3 item): one
   internal federated search planner shared by `context.manage` and
   `capability.manage`; capability search widens from case-sensitive
   name-contains to descriptor fields with provider-owned indexes
   (`TOOLS-10`); read-only search and the CTX-DISC-03 caps are enforced
   from the first prototype. No public `runtime.search` schema before the
   Phase 3 comparison.
   **Landed 2026-08-14.** Shared planner in `agent-contracts::discovery`;
   public tools unchanged. Comparison decision: keep the two manage
   surfaces; do not add `runtime.search` until a later measured need.
   Honest residuals: Artifact/Task/Agent/Skill/Event providers; inspect
   revision/`stale_revision`; `denied`. Search latency is now on the M15
   event-stream metrics (item 5), not a discovery-schema residual.
5. [x] M15 retrieval metrics — search recall/latency, post-GC recovery
   success (found-after-forgotten rate), reinforcement distribution — and
   runs the paired real-workload comparison (Phase 4). The first paired
   run does not wait for item 4: it may run on the current catalog + graded
   access baseline; later runs measure the discovery effect.
   **Landed 2026-08-14 (instrumentation + engine baseline; coding gate still
   open).** `RunMetrics` aggregates search/inspect/fetch/admit, miss
   reasons, search latency from envelope timestamps, forgotten/recovered
   ids (GC eviction + `externalized_ids` joined to search descriptors),
   and final graded-access stamp counts from diagnostics.
   `agent-eval --retrieval` is the catalog + graded-access found-after-
   forgotten baseline. `--compare-arm` now prints retrieval rows (coding
   fixtures usually stay at zero searches). This does **not** close the
   paired real-model coding acceptance or Phase 4.

This slice is closed 2026-08-14. The typed user-input envelope
(`CTX-EVENT-01..03`) is landed below with residuals. Do not reopen Phase 4
from here.

### Completed slice (2026-08-14) — typed user input (`CTX-EVENT`)

1. [x] `CTX-EVENT-01` dialogue envelope + keep cancel/command direct.
2. [x] `CTX-EVENT-02` `Applied` / 1-slot `Queued` / overflow `Rejected` /
   `InterruptCommitted` after `TurnCancelled` / `Consumed` then `Archived`.
   Queue is in-memory only; `Received`/`Interpreted` unused.
3. [x] `CTX-EVENT-03` bounded preview + `user-input` artifact; replay reads
   `body_ref` when a workspace is supplied.

Do not interpret user authority from prose, do not add `runtime.search`,
and do not start PLAT-05+. (The shared exclusion list now lives in the
current slice above.)

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
  **Retrieval metrics landed 2026-08-14** — search/inspect/fetch/admit
  counts, miss reasons, search latency, found-after-forgotten
  (`externalized_ids` ⨝ search descriptors), and graded-access stamp
  totals. `agent-eval --retrieval` is the engine baseline; `--compare-arm`
  prints the same rows from the event stream. Paired real-model coding
  acceptance stays open.
  **Resident bytes landed 2026-08-14** — `ContextDiagnostics.resident_bytes`
  is the UTF-8 size of Resident heap bodies; `RunMetrics` keeps last/max
  pre-model samples; `agent-replay --compare` prints final/preview-peak
  `res_bytes` / `peak_bytes`. The 10,000-turn episode fixture asserts byte
  flatten. The first heavy replay omitted turn-boundary `ContextGc` and
  incorrectly left C's heap ≈ A. With actor-parity GC, C's heap is a small
  fraction of A (`long_refactor` 69 332 → 4 298). Active-task latest-file-body
  policy then restored `fn handle_21()` on turns 22–24 (required **4/4**)
  without growing Resident bytes or leaking forbidden facts. Scoring stays
  frozen; turn-boundary GC stays on.
- [x] Replace the constant marker with a deterministic rolling fold baseline
  and count its visible derived-content cost.
  **Done 2026-08-12.** `context-baselines` gained a `Summarizer` trait
  (injected via `RollingSummaryEngine::with_summarizer`) plus a bounded
  fold-digest (`SUMMARIZER_PRIOR_CAP = 2 000` chars), so the rolling marker
  reflects the folded content instead of a constant placeholder; the eval
  harness injects a deterministic `ScriptedSummarizer` and the cross-engine
  comparison now folds on the fixture workload (the default 9 000-token
  threshold would never fire — the whole run stays near 300 tokens — so the
  rolling arm uses 200/100 thresholds and folds from the fourth turn).
  This is a real bounded fold mechanism with a scripted summarizer, not a
  competitive model-backed compactor; the latter remains a Phase 4/M15 arm.
  **Follow-up 2026-08-15 (EVAL-01.5.p1b).** The scripted digest stays the
  CI arm. Live B now injects the shared `ModelBackedCompactor`; C uses
  the same operator on `TaskCompleted` as a sourced distill, not a
  lossy fold. Compactor provider tokens are counted. Episode-rotation
  distillation remains open.
  **Follow-up 2026-08-16.** Episode rotation now distills with the same
  operator (`episode-derived` card, sources kept, prior card superseded).
  Manager/derivation cost is counted separately from the input-token gap:
  `manager_token_cost` re-materializes the final state and sums the
  `Summary`/`source == "derived"` items, surfaced as `EngineRun.
  manager_tokens` in the comparison table (measured: rolling folds 3-4
  records with a 26-token marker at ~12 960 model_in vs append ~13 220 vs
  dynamic ~12 090 on the same five-turn script; append/dynamic inject zero
  manager tokens — the fixtures never complete a task or derive).

### Phase 1 — Correctness before smarter policy

- [x] Preserve the latest body for the current file/entity across the
  `long_refactor` turn-23 window while keeping turn-boundary GC enabled.
  **Done 2026-08-14.** Active-task latest-file-body roots (path-only first
  line, cap 8, same-path reread supersedes, task switch drops the set)
  make `long_refactor` required facts **4/4**; forbidden facts stay 0 on
  C; Resident bytes on heavy scenarios remain a fraction of A. Scoring
  and `active_threshold` / `archive_threshold` were not retuned.
  **Follow-up 2026-08-15.** Live `fs.read` bodies are numbered lines, so
  latest-file-body and catalog search use structured `file_path` /
  `file_revision` from tool metadata. Replay `path:\nbody` remains a
  fallback. Do not treat `tool:fs.write` / `tool:edit.replace` as file
  bodies.

- [x] Verify the `TaskToolRequirementSet` first slice:
  actor-serialized whole-set CAS, `MustSurface`/`PreferSurface`/`KeepReady`
  round planning, bounded decision events, runtime surface revision, and
  the requirement slice now carried by RuntimeCheckpoint v4 restore/resume.
  Full workspace tests and strict Clippy
  pass. This historical item proves only the tool-demand subset; the complete
  TaskAnchor is implemented and tracked by its separate items below.
- [x] Make live-restore rebasing a durable, bounded audit transaction
  (`CORE-03`): `RuntimeEvent::RuntimeRestored` carries typed
  old/restored/effective revision data and a capped rebased-task sample;
  an audit failure after restore commit leaves aligned restored state but
  sets `RecoveryRequired` and fences further mutation.
- [x] Replace free-form task completion Summary with a typed `TaskOutcome` /
  `CompletionRecord` carrying task id, anchor revision, a deterministic
  completion-summary ref/digest, and bounded artifact refs (`CTX-10`).
  The complete final assistant response is a separate attached artifact when
  storage is wired; it has no dedicated raw-body digest yet. Acceptance results,
  verification, typed outcome status, and unresolved-state snapshots remain
  richer completion-contract work.
- [x] Persist the exact final response before ContextItem truncation; stop
  treating task-less Session Durable summaries as the authoritative result.
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
  empty still runs and ages external entries (`gc/full/`: "An
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
- [x] Introduce canonical `ContextCatalog` ownership; body movement changes
  location only, never lifecycle authority (`CTX-02` structural target).
  **Authority-isomorphism step landed 2026-08-12.** `ExternalizedContext`
  now carries the item's full authoritative lifecycle metadata (importance/
  relevance, real `created_tick`/turn clocks, access count, GC generation,
  eviction tick) captured at externalize time — externalization no longer
  degrades authority, and `inspect` projects the real values instead of
  zeros or the externalization tick. Body movement no longer rewrites
  authority: `reenter_working_set` stopped clobbering `created_tick`/
  `created_turn` on admit, matching the GC reactivate path.
  **Directory + query indexes landed 2026-08-14.** `ContextCatalog` is the
  `item_id -> location` directory over heap / warm buffer / external map,
  with task/scope/kind/entity/label/residency/attention indexes. GC recall
  and `context.search` consume those buckets; `label` is a real
  `ContextSearchQuery` dimension. Authority metadata stays on the single
  body (not copied into a second record). Checkpoints serialize the three
  stores and rebuild the directory. A free-text needle that hits no
  entity/label key still residual-scans summaries/uris. Duplicate-ownership
  detection remains a three-store check: the catalog skips a duplicate on
  rebuild and is not the fence. Regressions:
  `catalog_assigns_exactly_one_location_per_id`,
  `stored_search_ids_use_label_and_entity_indexes`,
  `external_retrieval_searches_inspects_and_fetches` (label filter).
  **Graded access (`CTX-GC-11`) landed 2026-08-14.** Search/inspect/fetch/ack
  stamps write through the stored body (`AccessSignal`,
  `search_reinforce_count`); the catalog stays a location directory. Search
  is the weakest signal and cannot pin Cold entries after one aging delay.
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

- [x] Pass a bounded active `TaskAnchorView` and typed root claims through
  materialization/GC without duplicating TaskManager authority.
  **Landed 2026-08-12.** The runtime projects the active task's anchor root
  claims (`working_refs` + `evidence_refs`) into a bounded
  `ContextHints.anchor_roots` on every materialization (PromptRequired
  forces the target into the model frame) and pushes the same projection as
  a `ContextAction::AnchorRoots` whole-set replacement before GC/Storage GC
  (ResidentRequired/PromptRequired protect or recall working-set entries,
  StorageRequired protects store retention; the completion boundary force-
  clears the projection so finished work is no longer rooted). The engine
  consumes claims by item id, `context://run/<id>` uri, or exact entity
  signature, never resurrecting terminal semantic state. Task authority
  stays with the TaskManager — the engine only ever sees the bounded
  projection (`MAX_ANCHOR_ROOT_CLAIMS`). Covered by engine unit tests
  (directive replacement/bounds, GC protection, buffer reactivation,
  terminal non-resurrection, prompt-required selection, storage protection),
  the process-boundary parity snapshot (directive + hinted materialize),
  and E2E `task_anchor_roots_are_projected_into_materialization_hints`.
  **Follow-up 2026-08-15.** `ContextHints.task` / `MaterializedContext.task`
  carry the bounded `TaskAnchorView`; see the split-roots item below.
- [x] Split mandatory-materialization, online-residency, and storage-retention
  roots; report `anchor_revision + source_field + RootReason` for each root.
  **Landed 2026-08-15.** `TaskAnchorView` is the bounded prompt projection
  (goal/interpretation/constraints/criteria/progress/open loops, no raw
  refs). The runtime copies it through `ContextHints.task` onto
  `MaterializedContext`; the engine does not score or own it, and the
  assembler renders it in the focus frame. `AnchorRootClaim` now carries
  `anchor_revision` + `RootReason` independently of `strength`.
  `PromptRequired` forces the model frame (and residency only because
  rendering needs the body); `ResidentRequired` is a GC/recall root and
  does not force selection; `StorageRequired` forbids permanent deletion
  and is not a residency root. GC/Storage GC reports list bounded
  `anchor_root_protections`. Active/Suspended downgrade and replacing the
  whole Focus subtree as a root remain the next items.
- [ ] `CTX-11`: implement Active/Suspended root downgrade and precise
  rehydration from `TaskAnchor + ResumePoint` without restoring the old
  transcript. Keep `TaskAnchor` as the only task-authority owner; store one
  actor-owned, revision-bound `ResumePoint` subrecord and expose only its
  bounded `TaskProgressView` to model context. The contract must cover current
  objective, unresolved constraints/blockers, next actions, checked file/entity
  refs with observed digest/revision, recent verification results, known failed
  commands, and evidence refs. Raw file bodies, command output and dialogue stay
  in artifacts/context storage and enter the view only through typed refs.
  Updates occur only at trusted safe points and must be idempotent/revisioned.
  Acceptance: oversized/corrupt restore fails closed; stale file digests are
  marked stale rather than reported as checked; a later successful verification
  resolves the matching failure; suspend -> unrelated work/GC -> resume restores
  the exact anchor revision and bounded progress while ordinary dialogue remains
  absent; repeated failures and thousands of inspected files cannot grow the
  checkpoint, resident heap or prompt without bound. Add a dedicated
  `task_switch`/failed-tool regression, then compare only after the tool
  preflight rerun establishes a clean baseline. Do not attribute reduced M15
  rounds to this change while wrong-shell, stale-edit or workspace-noise
  failures dominate; scoring remains frozen.
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
- [~] Replace whole-heap minor scans with dirty ids + bounded aging work.
  **Partial 2026-08-16.** Catalog sync is dirty-id incremental; minor
  residency and Cold aging honor `gc_work_batch` / `GcWorkCursor`. A
  heap at or below the batch (default 4096) still visits every item in
  stable order. Full GC sweep is still a whole-heap pass.
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
- [x] Emit a bounded metadata-only `WorkingSetSignal` at tool commit so the
  next model round sees newly hot files/symbols (`CTX-08`).
- [x] Bound materialization candidates and external preview tokens and fix
  fit-before-top-K (`CTX-07`). Keep candidate/materialize cost in the M15
  evaluation queue rather than reopening the correctness defect.

### Phase 3 — Navigation, trajectories, and handoff

- [x] Land the bounded `admit`/`derive` baseline with preserved identity,
  `DerivedFrom` provenance, quotas and runtime tests. Future work is authority
  grading, richer provenance and canonical-catalog ownership, not the basic
  operations.
- [ ] Introduce descriptor cards and stable content-digested `body_ref`s.
- [ ] Produce Task/Episode/Decision/Finding/OpenLoop/Artifact/Evidence cards
  from typed events; never infer runtime authority from prose alone.
- [~] Add task-completion trajectory distillation with verification and
  source refs; do not inject full old trajectories by default. Reuse the
  M15 model-backed bounded compactor (working-order item 3) as the
  distillation operator instead of building a second summarizer; inside
  C its output is a `derive`d item with provenance, never a lossy fold.
  **Partial 2026-08-15.** `TaskCompleted` distillation is landed when a
  `BoundedCompactor` is injected. Episode-rotation distillation is not.
  **Follow-up 2026-08-16.** Episode-rotation distillation is landed: same
  operator, `episode-derived` card with `DerivedFrom`, sources kept, prior
  card superseded. Compact failure does not fail the user turn.
- [ ] Add AssignmentCard/HandoffCard after single-agent episode semantics
  pass evaluation.
- [ ] Define independent storage retention profiles for coding, research,
  general assistant, and audit-sensitive runs.
- [x] Land `CTX-DISC-01..03` over Context + Tool only; compare separate
  `context.manage`/`capability.manage` surfaces with one merged discovery
  surface before replacing either public contract.
  **Landed 2026-08-14.** Shared internal planner; public contracts stay
  separate. No `runtime.search`. Artifact/Task/Agent/Skill/Event still out.
- [~] Land `CTX-EVENT-01..03`: typed user-input lifecycle, bounded event
  bodies, and source-authorized state patches. Do not route user authority
  through `ToolOutput`.
  **Partial 2026-08-14.** Envelope, bounded journal, optional artifact,
  1-slot queue, Rejected overflow, InterruptCommitted, Consumed/Archived,
  and `body_ref` replay are landed. Source-authorized state patches
  (`proposal` still `None` on dialogue) and crash-durable queuing remain
  open.
- [ ] Specify the managed-child lifecycle and `agent.manage` surface, but keep
  spawning disabled until the effect, sandbox, resource, and real-evaluation
  gates can enforce it.

### Phase 4 — Evaluation-gated tuning

- [ ] Persist one auditable result bundle for every intended live cell:
  run/cell/repeat id; commit + dirty-tree digest; provider/model, engine and
  budget config; immutable suite/fixture/prompt/verify hashes; arm order;
  complete event JSONL with sequence and usage-completeness validation; final
  workspace diff/hash and executable hidden-test evidence; machine-readable
  summary. Broadcast lag, missing usage, timeout and round-cap are explicit
  invalid/failure outcomes, never silently omitted rows.
  **Partial 2026-08-14 (EVAL-01.1).** Live `--compare-live*` writes a versioned
  `agent-eval.cell.v1` / `pair.v1` bundle per intended cell (manifest with
  fixture hash + git HEAD/dirty + `OPENAI_MODEL`/`OPENAI_BASE_URL` never the
  key; events.jsonl without `ModelDelta`; summary with seq-gap, broadcast
  lag, usage-incomplete, tool histogram; workspace sha256 not a copy;
  verify.json). Timeout/round-cap/runtime errors stay in the pair as
  `outcome=error`. `--show-evidence` rebuilds the table.
  **Partial 2026-08-14 (EVAL-01.1b).** Hidden checks persist as named
  file-content asserts plus bounded bodies (`agent-eval.verify.v1`);
  `--show-evidence` prints failing asserts, and the report can be re-run
  after the workspace is gone. Not pytest/build; the 300-task suite still
  needs executable hidden tests. Scoring of the five smoke fixtures is
  unchanged.
- [ ] Pre-register the formal paired analysis before collecting acceptance
  cells: at least 300 independent heterogeneous tasks, counterbalanced arm
  order, three within-task repeats, paired binary estimator, task-clustered
  one-sided interval, infrastructure-failure/timeout policy and power
  simulation. Report intent-to-treat end-to-end cost; do not exclude C's extra
  model/tool rounds as unfair noise.
  **Partial 2026-08-14 (EVAL-01.2 / EVAL-01.3).** Estimator, ITT rule, live
  arm-order shuffle and the 5000-sim tables are frozen in
  `agent-eval --preregister`. Historical 30×3 / −5 pp has only 961/5000 ≈ 19%
  power at Δ=0; EVAL-01.3 amends the gate to 300×3 (4048/5000 ≈ 81% at Δ=0,
  margin still −5 pp). The suite is not frozen; do not collect acceptance
  cells; do not invent 300 tasks.
  **Partial 2026-08-14 (EVAL-01.3b).** SPEC re-registered: `suite_frozen=true`,
  pack n=509, retrieval secondaries declared, gate n/margin unchanged.
  Do not collect 300×3 acceptance cells until the frozen ~30×3 calibration
  pilot.
  **Partial 2026-08-14 (EVAL-01.3c).** Exact 300 acceptance ids frozen
  (`7ff6b5dd…`); gate requires that set. Cost-eligible paired tokens
  replace the old ITT-token mean; cost-missing rate is reported
  separately. Task-residual corr(A,C) is the power-model diagnostic;
  pooled φ is confounded by task difficulty.
  **Partial 2026-08-14 (EVAL-01.4a).** `crates/agent-eval/suite/` is the
  reviewed-deliverable pack: freeze computed from n/provenance/executable
  hidden commands/heterogeneity/review flags. `--suite` reports n/300 and
  blockers. `manifest.frozen=true` with blockers fails closed.
  **Partial 2026-08-14 (EVAL-01.4b).** Two tasks harvested from this
  repository (`openai-wire-tool-names` @ `1f89e5b`, `uuid-parity-keys` @
  `0d2dd2f`): directory pack with model-visible `seed/`, verify-only
  `hidden/` overlay, and `expected/` self-check oracle. Seed fails
  `cargo test --offline`; the expected patch passes. Hidden tests are
  not in the seed.
  **Partial 2026-08-14 (EVAL-01.4c).** Suite is 9/300: added CPython
  `itertools.batched` / PEP 616 `removeprefix` (recall after distractor
  notes), TOOLS-09 Python symbol tests, vercel/ms negative-parse and
  minutes-shadow, plus this-repo JCS object canonicalization and
  search.grep cooperative cancel. Languages rust/python/javascript;
  classes bug/feature/refactor/test/recall; sizes small/medium/large;
  one notes-then-reuse task.
  **Partial 2026-08-14 (EVAL-01.4e / EVAL-01.3b).** Pack frozen and
  `SUITE_FROZEN=true`. SPEC re-registered: retrieval secondaries declared,
  n/repeats/margin unchanged. Do not collect 300×3 acceptance cells
  until the frozen ~30×3 calibration pilot.
  **Partial 2026-08-14 (EVAL-01.3c).** Exact 300 acceptance ids + hash;
  509 is not an optional pool.
  **Partial 2026-08-14 (EVAL-01.5).** Frozen 30-id sample + `--pilot-run`
  / `--pilot-calibrate`. File-only live 9×3 (81 cells) collected under
  `crates/agent-eval/evidence/pilot-30`; `decision=pilot`, gate ineligible.
  **EVAL-01.5.p1 (2026-08-15).** Remaining P0 SWE-bench skipped; send vs
  pack split + shared 48-round live cap. P1 SWE-bench n=1 diagnostic
  collected under `target/eval-evidence/p1-swebench-diag` (C 3/3, A
  round-cap, not an ITT table). Do not mix P0/P1 tables.

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
- [x] Explain the live C round inflation seen in `recall_after_fix` using the
  persisted traces (empty searches, repeated reads, failed calls, tool-surface
  and prompt differences) before claiming an end-to-end token saving.
  **Done as a diagnostic, 2026-08-14.** Bundles in
  `target/eval-evidence/reasonable-live-retry`: C's extra rounds were a
  no-tool first turn, then failed `git.status` / `shell.exec` probes and
  extra rereads — not empty search. n=1; scoring stays frozen; this is not
  an end-to-end token-saving claim and does not close M15.
  **Follow-up 2026-08-15 (EVAL-01.5.p1c).** Catalog-wide search/inspect
  landed; prompt stuffing (cache/optional/how-to) was removed rather
  than replaced with a longer tutorial.
  **Re-measure 2026-08-15 (n=1).** pep616 extreme C inflation converged;
  js-ms C extra tools gone; `recall_after_fix` extra rounds remain
  (failed probes/rereads, no catalog search). File-only 9 + recall
  complete under P1 n=1; leftover extra rounds are mixed (recall,
  js-ms-negative) not universal. Empty-assistant flake on three cells.
  **P1 SWE-bench n=1 (2026-08-15).** `target/eval-evidence/p1-swebench-diag`,
  pre-path-stamp binary. C 3/3 pass (24–27 rounds); A 0/3 at 48-round
  cap; B mixed (one `usage_incomplete` flake). C catalog search 0.
  **P1 after-path n=1 (2026-08-15).** js-ms-negative C extra rounds gone
  this cell (9r/17t vs A 11r/18t, C search 1/2). `recall_after_fix` C
  still 21r/30t search 0; leftover is failed `git.status` / rereads.
  A verify-failed on distractor notes. Do not mix with the P0 81-cell table.
  Extra rounds are still a treatment effect. No scoring change. Does
  not close M15.
  **P1 after-anchor n=1 (2026-08-15).** `p1-after-anchor` on the
  TaskAnchorView binary. recall A/B/C verify-failed (`4B`); C 16r vs A
  13r, search 0. js-ms-negative C 10r pass ≈ A 11r, search 0; B hidden
  fail. Do not retune scoring. Do not close M15.
  **P1 after-episode-distill n=1 (2026-08-16).** `p1-after-episode-distill`.
  recall: B pass 15r; A/C `verify_failed` both 17r (C missing `4B`; A
  missing Breville/200); C search 0 recovered 5/18. js-ms A/B/C pass
  (C 6r = A 6r). Extra C rounds on js-ms stay gone; recall leftover is
  notes + failed `git.status`. Not an ITT table. Do not retune scoring.
  Do not close M15.
  **P1 file-only 9×3 (2026-08-16).** `p1-file-only-calibrate`. ITT
  A=B=C=0.889; `decision=pilot`; analyze ineligible n=9 LCL=0
  `degenerate=true`. `uuid-parity-keys` 0/9; other 8 9/9. Cost-missing
  0/27. Not mixed with `pilot-30` or after-proxy. Do not retune scoring.
  Do not close M15.

## Acceptance properties

### Long-task boundedness

- Resident bytes and candidate count flatten with current episode plus live
  unresolved state across 10,000+ turns.
  **10k episode fixture (2026-08-14):** item count and Resident bytes both
  flatten (peak bytes asserted `< 80_000`, turn-10 000 vs turn-2 000 growth
  capped). **Coding replay (2026-08-14, after turn-boundary `ContextGc`):**
  C prompt tokens and Resident heap bytes both drop vs A (`long_refactor`
  bytes 69 332 → 4 298). Active-task latest-file-body policy (2026-08-14)
  keeps `fn handle_21()` in view (required 4/4) without putting the heap
  back. The 10k dialogue fixture is not a substitute for that split.
  Do not disable turn-boundary GC.
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
- Before claiming non-inferiority, use paired coding runs (minimum 30
  independent tasks × 3 within-task repeats) and pre-register the paired
  binary estimator, task clustering, one-sided confidence construction,
  infrastructure-failure policy and a power simulation. The initial bound is
  a 95% lower confidence limit on C − A no worse than −5 percentage points.
  Repeats do not increase the independent task count; a 3-run paired smoke is
  enough only to validate the harness.

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
2. [x] Ratify `ResourceRef`/`ResourceDescriptor`, provider revision semantics,
   and the hard limits for one federated search round.
   **Decided 2026-08-14.** `resource://v1/<kind>/<id>[@rev]`; Context + Tool
   only; fanout 2, 32 rows, 4000 result chars, 256 query chars, 8 searches
   per turn, identical-query budget 2. Revision is optional (`gc_epoch` /
   catalog generation when known); inspect-by-id does not yet request a
   revision.
3. [x] Decide whether v0 adds a new `runtime.search` surface or first implements a
   shared internal planner behind existing `context.manage` and
   `capability.manage` controls.
   **Decided 2026-08-14.** Internal planner only; no public `runtime.search`
   until a later Phase 3 comparison against measured need.
4. Ratify the independent catalog/schema, invocation/effect, host-process,
   and managed-child lifecycles.
5. Ratify child budget inheritance, cancellation, parent root transfer, and
   the minimum AssignmentCard/HandoffCard fields.
6. Choose per-event GC/search work budgets and acceptable deferred-work
   backlog semantics.
7. Extend M15 with search precision/cost, stale-resource rejection, steering
   latency, child cancellation, handoff fidelity, and parent task-success
   metrics before enabling broad multi-agent execution.

Implementation of the Context + Tool discovery prototype is landed; the
typed user-input envelope (`CTX-EVENT-01..03`) is landed with residuals
(in-memory queue, no NL-inferred patches). Managed-child execution
remains gated on trusted effects, sandboxing, resource policy, and real
evaluation.
