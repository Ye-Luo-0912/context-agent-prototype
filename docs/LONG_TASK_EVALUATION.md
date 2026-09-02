# Long-Task Runtime and Development Evaluation

This document defines the next development diagnostic after the retained
longflow r8-r10 result. [`ROADMAP.md`](ROADMAP.md) remains milestone authority;
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) remains the runtime-state
contract. This plan does not reorder M12/M13, retune Context, or close M15.

## Why the current longflow is not the next benchmark

`late_constraint_long` is a useful 15-directive diagnostic. It measures
cross-turn constraint retention, tool-surface churn, editor recovery and
TaskProgress lightness. It does **not** measure an autonomous development
task: the user decomposes the work into 15 small instructions, and every
instruction creates a durable turn boundary for the agent.

The next benchmark must instead start from one complete development request.
The model chooses the investigation, plan, edits, verification and review. A
separate interrupted twin must stop only at a runtime safe point, restore a
new runtime instance, and continue without replaying the original transcript.

## What r6-r10 established

The high C round/call count was not caused by a larger selected Context:

1. A successful explicit tool load lived for one result-delivery decision, so
   loading `git.status` displaced a still-unused `git.diff`. The pending-load
   cohort now keeps exact explicitly loaded siblings until each is used,
   unloaded, or the directive ends.
2. Compact universal coding tools were catalog-controlled. The model spent
   whole decisions searching/loading `fs.write`, `git.status` and `git.diff`.
   Keeping those small schemas on the stable surface removed that control
   loop without changing effect authority.
3. Additions were expressed as replacements. Sequential hunks could consume
   a later hunk's anchor. Explicit `replace` / `insert_before` /
   `insert_after` operations now preserve insertion anchors and keep
   revision/exact-match refusal fail-closed.
4. Ordinary final answers and durable task closure had been coupled. Repeated
   `task.complete` calls closed task affinity and forced rediscovery. A final
   answer now ends a turn; only an explicit, gated completion proposal closes
   the long-lived task.

Across stable-core r8-r10, the median C-A gap is +1 model round and +4 tool
calls instead of r7's +16/+24, while C retains the much smaller historical
Context and resident working set. The remaining r10 failures are locator or
verification outcomes, not evidence that Context should be enlarged or that
filesystem settlement is unsafe.

## Runtime state used by a long task

Do not introduce a second task table or another `ResumePoint`:

| State | Owner | Contents | Model exposure |
| --- | --- | --- | --- |
| `TaskAnchor` | Runtime `TaskManager` | goal, interpretation, constraints, acceptance, plan, open loops, working/evidence refs | bounded `PERSISTENT TASK STATE` |
| `ExecutionState` (`TaskRecord.resume`) | Runtime | checked `path@revision`, latest verification, failed commands, obligations, negative facts, evidence frontier | bounded `TASK PROGRESS` |
| Context | replaceable `ContextEngine` | selected evidence and episode working set | budgeted Context Frame |
| `TurnFrame` | active turn | current model/tool exchanges and bounded receipts | retained tail plus deterministic turn checkpoint |
| Artifacts | workspace store | exact user bodies and large/raw tool output | refs and bounded reads only |

The combined `TaskAnchor + ExecutionState` already covers current goal,
constraints, checked files, verification and known failures. The missing
product slices are a bounded agent update for the next action/open loops and
safe-point persistence/continuation during a single long user directive.

## Target runtime flow

```text
one user development request
  -> create/activate Task + initialize TaskAnchor
  -> seed ActiveTurn from TaskRecord.resume
  -> BeforeModel revalidation
  -> RoundExecutionSnapshot
  -> materialize Context + assemble prompt + freeze tool surface
  -> model decision
       -> final response -------------------------------+
       -> tool batch                                    |
            -> trusted attribution / approval           |
            -> execute or prepare effect                 |
            -> generation fence + commit/rollback       |
            -> settle complete batch                     |
            -> update ExecutionState / obligations       |
            -> LONG-TASK SAFE POINT                      |
                 -> optional bounded progress proposal   |
                 -> coalesced durable resume checkpoint  |
                 -> next model decision -----------------+
  -> persist assistant result + Context maintenance
  -> durable TurnCompleted barrier
  -> install final TaskRecord.resume
  -> completion gate (only explicit task closure)
  -> final checkpoint / task scope close
```

A long-task safe point exists only when the whole requested tool batch has a
terminal settlement, every prepared effect is committed or rolled back, the
authority/event records required for that settlement have landed, and no
model/tool operation is in flight. It is state-driven, not `every N rounds`.

## Landed Runtime slices preceding the first live development pilot

### LT-RUN-01 — bounded task progress proposal

Reuse the already proposed Runtime-owned `task.manage`; do not add another
memory system. Its first slice is catalog-cold by default and becomes a task
requirement for explicit long-task runs. A progress call uses
`base_anchor_revision` CAS and may update only bounded autonomous fields:
current interpretation, plan progress, open loops and one explicit
`next_action`. Goal/constraint changes remain the existing boundary/approval
path. The proposal stores identities and short text, never file bodies, raw
tool output or transcript excerpts.

`next_action` is replaceable guidance, not a planner or execution authority.
The LLM still decides the next call, may revise the action after new evidence,
and is never forced to predeclare a fixed number of steps. A task can complete
without a progress call when it is genuinely short.

Acceptance:

- stale anchor revisions fail without changing task state;
- the prompt projection remains bounded and contains each field once;
- checkpoint/restore preserves the proposal exactly;
- a model cannot edit user constraints through this operation;
- no progress body is copied into Context or transcript history.

Status: landed deterministically 2026-08-24 (tool bounds, accepted and
stale actor paths, checkpoint round-trip, single-render projection).
This slice made no standalone live claim; the later safe-point, completion and
pilot results are recorded below.

### LT-RUN-02 — safe-point resume commit and checkpoint policy

This subsection records the historical design that led to the actor-owned
safe-point path; it is not the current event contract. The current turn commit
is `TurnCompleted + RuntimeCommitBarrier(Turn)`, and the durable continuation
substrate is summarized in
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md#long-task-continuation-boundary-durable-substrate-landed-autonomous-segmentation-deferred).
The TUI/evaluator never becomes a second orchestrator.

At a fully settled batch, meaningful durable changes accrue bounded
`CheckpointDebt` reasons such as task-anchor change, durable workspace
mutation, verification change, task suspend/explicit pause, completion or
shutdown. Read-only exploration alone does not synchronously checkpoint every
round. Reasons coalesce, so several mutations in one batch create one
candidate snapshot.

- Mutation/verification/task-boundary debt installs the bounded
  `ExecutionState` into the existing task resume and schedules one atomic
  checkpoint write after the safe point.
- Required contract: explicit pause, suspend, completion and clean shutdown
  wait for the durable checkpoint acknowledgement. A background checkpoint
  failure keeps the debt visible and retryable and must not report the task as
  safely resumable. The current continuation gap against that contract is
  stated in the status paragraph below.
- A complete externally captured checkpoint contains the task, Context,
  capability and authority-marker planes. The current safe-point background
  artifact was actor-owned and omitted the host capability plane; `LT-RUN-04`
  added capture plumbing, while the stable-generation ownership proof is
  reopened under `LT-RUN-05`. Neither form may
  serialize raw transcript history or an in-flight prepared effect.
- Restore reuses the existing validation/fence. A new
  `continue_active_task` runtime command starts a fresh `ActiveTurn` from the
  same task id, current directive, anchor and resume state; it does not mint a
  user instruction or increment the directive identity.

The implementation must add explicit events for progress update, safe-point
resume commit, durable checkpoint acknowledgement/failure and continuation
start. Eval must be able to prove their order from JSONL.

Historical landing status (2026-08-25): accrued debt reasons are task
anchor change, durable workspace mutation and verification change; they
coalesce into one resume install plus one atomic write under the workspace
state directory (`CheckpointStore`). `TaskResumeCommitted` precedes
`CheckpointDurable`, which lands before `TurnCompleted`; a failed write
re-arms the debt and emits `CheckpointWriteFailed`. Completion and
continuation wait for an in-flight write to settle, but continuation does not
yet reject the failed-write outcome. The background file is atomically visible
through write-plus-rename, not yet a checksummed/fsync cold-restart artifact.
`continue_active_task` restarts the stored directive with a
`task_continuation` input kind — no new dialogue identity, no re-ingest,
and a `TaskContinuationStarted` event. Deterministic coverage: ordering,
read-only no-debt, store atomicity, and continuation identity. The 2026-08-27
audit supersedes the implied durability claim: failed/debt/in-flight gating and
snapshot identity are reopened under `LT-RUN-05`.

### LT-RUN-03 — completion and recovery gate

Keep the landed final-answer/task-closure split. A claimed successful
completion is accepted only after the sibling batch settled, every required
obligation is resolved, required verification is Current, and no effect plane
is `Unknown`/`RecoveryRequired`. Runtime attaches its own assistant and
verification refs. Partial/failed completion may retain explicit open loops;
success cannot silently erase them.

The current terminal transaction is:

```text
TurnCompleted + RuntimeCommitBarrier(Turn)
→ prepare rollbackable post-completion Context
→ prospective post-Context terminal checkpoint durable
→ infallible task/focus commit
→ TaskCompleted + maintenance + RuntimeCommitBarrier(TaskCompletion)
```

The validated terminal checkpoint is authoritative in the
checkpoint-to-audit crash window. This does not make `task.complete` itself an
authority source.

Historical landing status (2026-08-25): the acceptance gate re-runs at
every completion edge: no recovery fence, no unsettled cancelled operation,
zero open failure obligations, required verification current, and no open
loops silently erased. A gated proposal on the one-shot path returns the
decision to the model with one warning per turn instead of committing;
the deferred and `/done` paths fail their transaction with the typed
reason. Success orders `TurnCompleted` -> final `CheckpointDurable` ->
`TaskCompleted`, so JSONL proves event order, not restore validity. The current
implementation has already cleared task authority when a failed final write is
reported; its warning cannot make that task active again. The 2026-08-27 audit
rejects that as the target contract. `LT-RUN-05` replaces it with the two-phase
completion protocol below. Deterministic
coverage: refusal with later resolution, ordering proof, and store
failure semantics.

## First concrete development task: `retry_policy_dev`

Use a frozen, network-free Rust fixture rather than this repository itself.
The seed is a small job runner with `Cargo.toml`, five to eight source/test
files and a README. The public API already exists; the incomplete retry policy
is spread across configuration, errors and execution.

One user directive asks the agent to:

> Implement a configurable bounded exponential retry policy. Retry only
> transient errors; permanent errors return immediately; `max_attempts`
> includes the first call; delay growth saturates at `max_delay_ms`; preserve
> the public `run_job` signature; add unit/integration coverage, update the
> README, run the project checks, review the diff and report the result.

The fixture uses an injected/fake sleeper, so verification has no real waits
or network. The target harness-owned oracle must validate boundary values,
overflow saturation, permanent-error behavior, public API compatibility,
README semantics and the allowed workspace diff. The current post-run
`cargo test` executes tests inside the agent-editable workspace and is only a
self-check. `LT-RUN-04` added an external-oracle path, but its setup and outcome
classification are reopened under `LT-RUN-05`. Multiple correct implementations
must be accepted; a golden text patch is not the oracle.

Two modes share the exact seed and directive:

- `normal`: uninterrupted end-to-end execution;
- `resume`: after the first durably settled workspace mutation creates
  checkpoint debt, the harness waits for its checkpoint, stops the runtime,
  constructs a new runtime, restores, and calls `continue_active_task`.

The interruption trigger is a semantic event, not a fixed round number.

## Evaluation layers

Do not vary model, Runtime policy and tool surface together; each layer names
its one comparison variable.

1. **Deterministic runtime gate:** scripted decisions cover progress CAS,
   safe-point ordering, checkpoint failure/retry, stop/restore/continue,
   no duplicated effect commit and completion refusal with stale verify.
   Historical mechanism status: green for `retry_policy_dev` as of 2026-08-25.
   The scripted
   normal/resume gate (`agent-eval --long-task-gate`, also run as a cargo
   test) drives two real runtime instances over the production tool
   surface — progress CAS at the operation commit, safe-point ordering,
   stop/restore/continue through the shared durable authority lineage,
   exactly-once byte-exact effects, deterministic marker acceptance and
   `TurnCompleted` -> final durable -> `TaskCompleted` ordering.
   Directive tools are leased from their catalog-cold state through
   `capability.manage`, exactly as a live run must. Checkpoint
   failure/retry and completion-refusal-with-stale-verify remain covered
   by the LT-RUN runtime slices. The 2026-08-27 audit adds same-anchor snapshot,
   exact ack correlation, final cold-restore and cross-consumer verification
   regressions before this layer can serve as an exit gate.
2. **C live development pilot:** `normal` and `resume`, two repeats each,
   using one pinned model/provider. These four cells validate the harness;
   they are not acceptance.
   Status: first execution 2026-08-25 (evidence
   `crates/agent-eval/evidence/retry-pilot/REPORT.md`). Harness validated
   the intended operator stop/restore/continue path. All four canonical
   cells FAIL lifecycle closure; the evaluator currently returns at that
   failure before running the post-run diff/cargo acceptance, so there is no
   canonical behavioral-pass claim. Two additional attempts are retained as a
   separate provider-transport failure window. No acceptance claim and
   nothing retuned.
3. **Completion-candidate isolation:** after `LT-RUN-05` deterministic gates,
   freeze the final Runtime/evaluator substrate and run retained-C
   CompletionOpportunity off/on normal/resume pairs, at least two repeats per
   mode. The candidate is default-off and is the only variable. Roadmap item 8
   gates promotion on behavior/outcome, median round/call and tail results.
4. **Agent/runtime comparison:** only after candidate promotion, freeze that
   setting and use the same pinned model with A/C context engines,
   normal/resume, two repeats (eight paired cells). This isolates Runtime and
   Context behavior.
5. **Model comparison:** freeze the retained C runtime and tool surface, then
   vary models on the same tasks. This measures model tool-use quality, not a
   Context algorithm change.
6. **Pack expansion:** only after the first task is green, add one diagnosis
   task and one multi-file migration task. Do not jump from one task to the
   frozen 300x3 M15 gate.

The cell counts above are experimental design. Runtime termination remains
state-based: verified acceptance, resolved required blockers and explicit
completion. Round/call/token/time values are safety ceilings, never target
step counts or automatic declarations of success.

## Implemented candidate phase: `LT-RUN-04` — trustworthy closure and cold continuation

`LT-RUN-01..03` established the bounded task/resume substrate. The first live
pilot changes the next implementation order; it does not justify a Task DAG or
a new planner.

### Evidence baseline and decision

The retained C pilot has four canonical cells:

| cell | model rounds | stop/restore/continue | `TaskCompleted` |
| --- | ---: | --- | --- |
| normal r1 | 13 | n/a | no |
| normal r2 | 24 | n/a | no |
| resume r1 | 6 + 17 | yes | no |
| resume r2 | 6 + 15 | yes | no |

All four event streams contain zero direct `task.manage` calls and zero direct
`task.complete` calls; none of the four canonical cells even loaded either tool
through `capability.manage` — their catalog-control calls fetched
shell/process/edit tools instead. The model reached a final report after
implementation, verification and documentation work. One retained earlier
attempt loaded and
called `task.complete`, closed the task and passed the post-run cargo check; its
cell failed only on the since-fixed Windows diff-path bug. The immediate
measured lifecycle gap is therefore in closure affordance/discoverability, not
evidence that the model lost a subgoal because no structured TaskGraph exists.

The resume cells prove a narrower property than cold process recovery: a new
Runtime instance can restore an externally captured full checkpoint and
continue the same directive. Phase two currently reuses the same
`ContextEngine` object, and the checkpoint passed to restore is captured after
the operator-style cancellation rather than loaded from the safe-point
artifact. `CheckpointStore` currently has no matching artifact-load API. Do not
claim full cold recovery from these cells.

The phase objective is:

> Make behavioral truth, explicit task closure and cold continuation separately
> observable; give the model a conservative closure opportunity only when
> Runtime has positive terminal evidence; and make a segment checkpoint a
> complete, revision-acknowledged Runtime artifact.

### Invariants and non-goals

- `RuntimeActor` remains the only task/turn orchestrator. Eval may request an
  interruption, but it does not assemble lifecycle state or schedule the next
  model decision.
- Keep one `TaskRecord`, the existing `TaskAnchor` and
  `TaskRecord.resume: ExecutionState`. Do not add another ResumePoint, global
  transcript or authoritative task table.
- Keep Context selection/GC, the retained tool surface and editor semantics
  fixed. A fresh engine in a resume twin validates restore; it is not a Context
  algorithm experiment.
- Production surface rev v5 keeps the compact `task.complete` schema visible.
  This grants no automatic task completion or closure authority, and no
  natural-language classifier guesses whether a final answer means whole-task
  closure; the Runtime-owned acceptance gate remains decisive.
- Do not implement a model-visible structured TaskGraph, Completion Proof
  Ledger, semantic-cycle control, memo, rewind or child agents in this phase.
- M12 then M13 remains the engineering mainline; this research slice neither
  closes nor reorders either gate.

### Slice A — preserve every outcome dimension

The live evaluator must not stop scoring merely because lifecycle closure is
missing. For every cell with a usable workspace, independently record:

```text
behavioral oracle: pass | fail | not_run(reason)
allowed diff:      pass | fail | not_run(reason)
task closure:      completed | active | failed(reason)
continuation:      n/a | restored | failed(reason)
provider/runtime:  healthy | failed(class)
```

The target final cell verdict is the conjunction of all mandatory dimensions;
separating dimensions does not turn partial work into PASS. It prevents a
closure failure from erasing whether the implementation was behaviorally
correct. Provider, verifier, safe edit, filesystem settlement and lifecycle
failures remain different classes.

Run read-only diff and behavioral acceptance whenever the final workspace is
inspectable, including after missing closure or a provider failure that occurs
after edits. `not_run(reason)` is reserved for an unavailable workspace or an
oracle that cannot start, not for failure in another dimension. Run and report
the workspace's own `cargo test` separately as an agent-authored self-check
before oracle injection, or target it so the injected oracle cannot execute.
Add a harness-owned, network-free external test crate (or equivalent isolated
oracle) for retry bounds, transient/permanent behavior, delay saturation and
overflow, and public API compatibility; keep README semantics and allowed diff
as harness-owned checks. Do not use implementation markers or a golden patch
as behavioral acceptance.

For resume cells, phase two must construct a fresh Context implementation and
load the persisted checkpoint artifact. Reusing the phase-one engine object is
not a cold-resume result.

Status after the 2026-08-27 audit: partially landed and reopened. The report
shape contains separate fields and resume twins construct a fresh Context
engine with a checksum-verified loader. However, oracle injection can fail
before execution because its target directory is not created; four retained
`behavior=fail` cells are setup failures (`os error 3`), not behavioral
failures. Overall PASS does not yet require successful resume, healthy
provider/runtime and absence of runtime error. Per-phase counters are lost on
some failed driver returns. No retained live aggregate is decision-grade.

### Slice B — separate CAS revision from verification basis

Retain the current monotonic anchor revision as the compare-and-swap revision
for the whole bounded record. A stale plan update must still be unable to merge
across a concurrent user constraint change.

Give the existing verification obligation's `spec_revision` an independent
verification-basis meaning (or migrate it to an explicit equivalent digest);
today it mirrors the whole anchor revision. Goal, constraint and authoritative
acceptance changes advance that basis and invalidate dependent verification.
`plan_progress`, `open_loops`, `next_action` and root/reference maintenance
continue to advance the record CAS when changed, but do not by themselves make
a Current verifier result stale. Whether `current_interpretation` changes the
verification basis must be an explicit host policy decision, not an accidental
consequence of sharing one counter.

Bind verification facts, verifier-source leases, exact PASS reuse and
completion currentness to one shared tuple: task id, verification-basis
revision, directive revision, workspace revision, trusted verifier identity
and exact argument digest. ActiveTurn and durable task-resume updates must use
the same predicate. Version the checkpoint representation for the second
revision and reject an unsupported payload before mutating restored state;
never silently infer currentness from a progress-only CAS revision.

The 2026-08-28 slice left a real gap: model-routable `task.manage` cannot edit
acceptance criteria, the trusted `AnchorPatch` API classifies them as
authoritative, and a newly created task normally has no typed criteria. The old
“shadow-only” disposition is superseded by `ACCEPT-RECEIPT-01`: pre-M15 Runtime
must ingest bounded user/host-owned criteria or explicitly choose the
conservative no-criteria closure policy; the model cannot mint either.

Acceptance for this slice:

- plan/open-loop/next-action-only updates preserve Current verification;
- goal/constraint/authoritative-criterion changes make dependent verification
  stale;
- whole-record stale CAS still refuses without mutation;
- checkpoint/restore preserves both revisions and their binding;
- ActiveTurn, task resume, exact reuse and completion agree on currentness;
- completion records remain bound to the authoritative task basis.

Status after the 2026-08-28 review: the reopened items are landed. The
verification basis is a counter independent of the whole-record CAS revision
(`TaskAnchor.verification_revision`, synced to
`ExecutionState.verification.spec_revision` at commit). Facts, verifier
sources, `validity()` / completion, exact / domain reuse and the opportunity
key all read that basis, so progress-only CAS keeps a Current verifier
everywhere while authoritative movement stales it everywhere. Acquisition
criteria are authoritative boundary fields (the verdict the outcome is
measured against) and move the basis; model-derived criteria stay proposals
because `task.manage` cannot submit the field, so no criterion-level approval
gate is implied — only dependent verification is staled. `validity()` also
refuses Current when the last evidence row binds an older basis, so a basis
move can never silently read as current even without the `SpecChanged` side
effect. The exit regression covers progress-only movement, an
acceptance-criteria change and a checkpoint round-trip in one test asserting
consumer agreement (ActiveTurn validity, completion, exact reuse and the
derived opportunity). Still tracked with the cold-resume matrix: the
persisted offered-opportunity key accrues checkpoint debt and a crash-window
proof must show once-per-basis discipline survives recovery. At that date,
criterion origin/authority was defined only to the extent that accepted
criteria were authoritative. Its shadow-only ingestion/acceptance status is
superseded by the current pre-M15 algorithm and `ACCEPT-RECEIPT-01`.

### Slice C — conservative `CompletionOpportunity` candidate

`CompletionOpportunity` is a derived, advisory safe-point fact, not completion
authority and not persisted as an independently editable boolean. It lands
behind a host-policy switch that is disabled by default until the isolated
candidate gate below passes:

```text
eligible = existing completion gate would accept
           AND no completion proposal is pending
           AND task-relevant durable work exists
           AND positive trusted verification is Current
```

The first positive-evidence source is deliberately narrow: a task-relevant
durable mutation followed, under the same verification basis, directive and
workspace revision, by a Current trusted verification result with a positive
receipt identity. Explicit user/host whole-task closure intent retains its
existing independent surface path; it does not turn absence of evidence into a
derived opportunity.

An empty obligation/open-loop set alone is not positive evidence: a new task
must not become closure-ready before doing work. Failed/stale verification,
new task-relevant mutation, recovery, pending cleanup or a reopened obligation
retracts the opportunity.

Derive a body-free opportunity key from task id, verification-basis revision,
directive/workspace revisions, trusted verifier identity, exact argument
digest and verification evidence digest/ref. Persist only the last actually
offered key in bounded `ExecutionState`, so unchanged reads and progress-only
anchor edits cannot re-arm the same hint.

When eligible, Runtime may lease `task.complete` as `PreferSurface` for the
next decision and project one bounded statement that the whole task may now be
closed. The model still chooses whether to call it. The lease is not permanent,
does not pin Context and does not add a confirmation model round after an
accepted terminal call. Emit an explainable event containing the eligibility
decision, opportunity key/revisions and typed reason so replay/eval can
distinguish `not_ready`, `offered`, `called`, `ignored`, `refused` and
`completed`. One unchanged key is offered at most once; a relevant mutation
revokes it and a later Current verification may create a new key.

Deterministic negative cases are mandatory: initial task, read-only
exploration, mutation without verification, stale/failed verification, open
loop, unresolved obligation, recovery fence and cancelled-operation cleanup
must not obtain the lease from derived readiness.

Promotion is a separate gate, not part of the later Context A/C. After Slices
A, B and D are fixed in both arms, freeze a deterministic replay of an
already-satisfied task and run the retained C Runtime with this candidate off
versus on.
Use the same pinned model/provider, fixture, tool surface and final substrate;
run normal and resume with at least two paired repeats per mode. The candidate
is the only variable, and each immutable manifest records its setting and host
policy revision.

Promotion requires harness-owned behavioral/API/diff success and outcome count
not to fall, lifecycle closure to improve, lower median rounds and tool calls,
and no new p95/max-turn constraint tail. Report added prompt/schema tokens and
offers per opportunity key. If the gate fails, the candidate remains off and
cannot be smuggled into an A/C comparison; only a promoted, frozen setting may
be held constant across later Context engines.

Status: the mechanism landed behind a default-off host switch
(`RuntimeServices::with_project_completion_opportunity`) on 2026-08-25, not
promoted. Eligibility is a pure derivation mirroring the acceptance gate plus
the two positive-evidence conditions (task-relevant durable mutation with
`MutationResult` provenance; a trusted verification pass current on the exact
identity tuple). The body-free key is once-per-basis and only the last offered
key persists in bounded `ExecutionState`; the lease prefers `task.complete`
for exactly one decision, arms one bounded prompt statement through
`TaskProgressView`, and retracts on cancel/failure. Typed events distinguish
not_ready/offered/called/ignored/refused/completed. All eight mandatory
negatives are deterministic-green at the derivation level, and actor-level
tests prove the switch is silent by default and never leases an initial task.
The already-satisfied-task replay the promotion gate requires is frozen as a
deterministic off/on pair (`agent-eval --long-task-gate` also runs it): one
durable mutation plus one real registered-recipe `verify.run` pass under an
unchanged basis yields exactly one offer, and the leased decision closes the
task through `task.complete` alone; with the candidate disabled the identical
script emits zero opportunity events and never surfaces the tool. The item-8
off/on paired live promotion gate has run twice on 2026-08-25
(`evidence/opportunity-gate/REPORT.md`) and **failed to promote both times;
the candidate stays off**. Attempt 1 (8 cells) armed zero offers: discovered
verifiers are TaskScoped by design, so no live PASS ever carried an exact
identity. Attempt 2 registered a host opt-in source-read-only
ExactCurrentWorld recipe on both arms and proved the mechanism end to end in
live conditions — receipt-backed offers fired once per mode, and resume/on r2
executed offer -> leased call -> committed closure; the then-current report
marked its dimensions pass —
but the retained ratios and medians are not promotion evidence because the
evaluator and resume correlation are reopened below. Eligibility also accepts
any historical `MutationResult` without the shared verification basis, and the
offered key is written after safe-point capture without accruing checkpoint
debt, so crash recovery can re-offer it. The candidate remains default-off.
Do not run another live attempt until `LT-RUN-05` closes these prerequisites;
upgrading discovered general runners remains a separate gated decision.

### Slice D — one complete durable checkpoint boundary

The historical pre-`LT-RUN-04` baseline stored an actor-owned checkpoint with
an empty host-capability list, while only `RuntimeInstance::checkpoint()` used
a generation handshake. `LT-RUN-04` then wired automatic capture to the host
capability snapshot. The current defect is narrower: that automatic path does
not prove a stable generation before/after capture and can therefore be torn.
`LT-RUN-05` carries the reopened generation and validation proof. Centralize
external and automatic
safe-point persistence through one narrow full-plane coordinator; it only
captures, merges, writes and acknowledges, while `RuntimeActor` remains the
sole lifecycle orchestrator:

```text
fully settled safe point
  -> install bounded ExecutionState into TaskRecord.resume
  -> freeze before any next model decision
  -> capture actor + Context + authority-marker planes
     at the exact resume-state revision
  -> capture host capability plane under generation handshake
  -> merge and validate one complete RuntimeCheckpoint
  -> write unique checksummed temp + file sync + atomic rename
  -> parent-directory sync where the platform supports it
  -> acknowledge exact revision + artifact + digest + bytes
  -> publish safely paused/resumable
```

The host performs a mechanical snapshot/merge; it does not become a second
orchestrator. Platform-specific directory durability limitations remain
explicit, especially on Windows.

The attempted `LT-RUN-04` design replaced reason-vector inference with
watermarks named:

```text
resume_state_revision
required_durable_revision
durable_revision
```

Debt reasons remain useful observability. Its intended continuation rule was
`durable_revision >= required_durable_revision`. A write,
checksum, rename, host-plane capture or acknowledgement failure must not emit a
safe-pause claim or start `TaskContinuationStarted`; it remains retryable and
visible. A segment boundary forces the latest bounded resume knowledge into the
checkpoint, while ordinary read-only rounds remain off the synchronous-write
hot path.

The resume harness must retain only the acknowledged artifact locator, digest
and revision across phases, then drop the phase-one Runtime, host and Context
engine. Phase two constructs new instances, checksum-validates and loads that
exact artifact, and only then calls `continue_active_task`. An in-memory
`RuntimeCheckpoint` may not cross the boundary. Instrument instance identity in
the deterministic harness so this fact is provable rather than inferred.

Status after the 2026-08-27 audit: envelope checksum, sync/rename persistence,
corruption refusal, typed events and a fresh-engine load path are landed, but
the boundary contract is reopened. Automatic safe-point capture does not use
the external checkpoint path's stable capability-generation handshake. The
three watermarks alias task-anchor revision, which does not advance for every
workspace/verification snapshot and can decrease across tasks; continuation
does not also require no debt, no failed write and no in-flight write. The
completion path can acknowledge a checkpoint whose active-task authority and
`current_task_id` disagree, which restore validation rejects. Read size,
serialization and retained-artifact growth are not yet bounded.

### Slice E — evidence gate for proof and planning research

The deferred Completion Proof Ledger, typed Completion Frontier, segment/yield
policy and semantic-cycle shadow proposal remains the preferred later LongFlow
direction, but its inputs must be earned in order:

1. establish criterion ingestion, stable identity, origin and authority;
2. run criterion proof as reconstructable shadow state before it changes a
   completion outcome;
3. use typed transitions rather than an opaque scalar progress score;
4. add cycle observations in shadow before any strategy advisory or refusal.

User-supplied structured criteria or a trusted host/fixture manifest may mint
authoritative criteria. Model-extracted criteria are proposals only: they
cannot silently expand, weaken or satisfy user authority. Shadow CPL/Frontier
means event and metric output only — it does not enter the prompt, gate
completion, change tool selection or become a new Context root.

For segment/yield research, this phase adopts only one rule: any future segment
transition must cross the complete durability barrier above. A typed
`task.yield`, autonomous segment loop or cycle-driven strategy change remains
ineligible until cold restore is green and shadow evidence demonstrates need.

Likewise, first add the planned diagnosis and multi-file migration tasks with
the current bounded `plan_progress` substrate. A model-visible structured
TaskGraph becomes a candidate only if at least two independently frozen task
families show a material lost-subgoal, repeated-work or post-resume rework
failure after closure, provider and tool failures are separated. Its first
evaluation must be same-model TaskGraph-off/on, must not become completion
authority and must not create new Context roots.

## Next phase task: `LT-RUN-05` — durable resume truth and decision-grade evaluation

The 2026-08-27 post-landing audit changes the order again. `LT-RUN-04`
delivered useful primitives, but deterministic mechanism tests did not prove
that the acknowledged artifact is the latest internally valid snapshot or that
the live evaluator reports that fact correctly. Do not optimize completion
arming or expand the task pack until the truth chain is repaired.

The phase objective is:

> Make one exact, bounded snapshot identity flow from settled Runtime state to
> durable acknowledgement, cold restore, continuation and the final report;
> make every verification and outcome consumer agree on the same basis.

### Work package 1 — coalesced monotonic snapshot fence

Use an actor-owned snapshot sequence independent of `TaskAnchor` revision.
Persist and restore it as part of the existing durable Runtime/authority
lineage: an unrelated run may start a new sequence, but a continued lineage
cannot reset or move backwards across a task switch or cold restore. This is a
sequencing identity, not another editable ResumePoint and not a fixed-round
checkpoint policy:

```text
durable state change
  -> mark checkpoint debt (coalesce while work continues)
fully settled safe point + debt + no write in flight
  -> allocate S = previous snapshot sequence + 1
  -> freeze the bounded resume state represented by S
  -> required_sequence = S
  -> capture, validate and persist S
ack(lineage, S, artifact, checksum, bytes, capability_generation)
  -> accept only for the matching frozen snapshot
  -> durable_sequence = max(durable_sequence, S)
newer debt during write(S)
  -> retain debt and schedule a later S+1; never clear it with ack(S)
```

Continuation is allowed only when all are true:

```text
checkpoint_debt is empty
AND checkpoint_write_failed is false
AND no checkpoint write is in flight
AND durable_sequence >= required_sequence
```

Task-anchor CAS remains task-record concurrency control. Snapshot sequence is
Runtime durability order. Neither may impersonate the other. This keeps the
hot path adaptive: state changes accrue cheap typed debt, while actual writes
remain coalesced at safe points rather than following a fixed round count.

Status: landed deterministically 2026-08-27. The actor allocates `S =
prev + 1` per frozen snapshot independent of anchor revisions; each write
carries its exact captured-debt set and only that set is retired on ack;
`Resume(S) -> Durable(S)` order and per-artifact retirement are covered in
`tests/turn/safepoint.rs` (same-anchor retry under a fresh sequence,
cross-task cycles keeping allocation strictly monotonic, failed-write fence
until a retried capture lands). The acknowledged artifact persists the
allocator watermark and restore adopts it monotonically. Out-of-order or
foreign acknowledgements are structurally impossible behind the single-flight
writer, so no separate stale-ack code path exists to test.

### Work package 2 — one valid full-plane checkpoint path

Automatic safe points, explicit instance checkpoints and final completion use
one narrow capture contract:

1. freeze a self-consistent actor/task state for sequence `S`;
2. capture Context and authority-marker planes;
3. capture the capability plane with bounded generation-before/after retry;
4. merge and call `RuntimeCheckpoint::validate` before persistence;
5. enforce explicit header, payload and total artifact byte limits;
6. write the checksummed envelope, sync, rename and acknowledge the exact
   tuple; and
7. retain artifacts under a bounded policy while preserving referenced/audit
   checkpoints.

Terminal completion is explicitly two-phase. First prepare a prospective
terminal snapshot in which `TaskManager.active` and Runtime task identity are
both cleared while the live task remains completion-pending and retryable.
Validate and durably acknowledge that snapshot. Only then commit the matching
in-memory terminal transition and emit `TaskCompleted`. A failed write leaves
the live task active/completion-pending; it never reports completion and may
retry from the same authorized completion intent. Regression coverage must load
the acknowledged final artifact and restore it into a fresh Runtime + Context;
event order alone is insufficient.

Status: landed deterministically 2026-08-27. Safe-point capture, external
instance checkpoints and terminal completion now share ONE assembler with
the bounded capability-generation handshake; every persisted snapshot passes
`RuntimeCheckpoint::validate` before any byte is written. Terminal completion
is two-phase: the prospective post-completion task plane (active cleared, the
completing task Completed with its record) is frozen under a fresh sequence,
validated and durably acknowledged while live state stays untouched — only
then are the in-memory transition and `TaskCompleted` published. A failed
terminal write leaves the task active/completion-pending and retryable;
store-less compositions complete in-memory behind an explicit warning and
never claim resumability. The store enforces explicit header/payload/artifact
byte caps on both write and load and prunes to a bounded newest-window after
every durable write. Covered by the turn/safepoint suite (terminal artifact
loads checksum-verified with no active authority and its own sequence, ack
precedes TaskCompleted, blocked-store keeps completion pending) plus new
checkpoint store-contract tests.

2026-08-30 amendment: terminal preparation now includes the post-completion
Context plane and its portable rollback snapshot, not only a prospective task
table. The checkpoint carries serde-defaulted `event_cover_seq` and
`terminal_commit`; the terminal audit is one bounded batch ending in
`RuntimeCommitBarrier(TaskCompletion)`. Failure before acknowledgement restores
Context, leaves the task active and retains a bounded `CompletionCommitFailed`
fact. Failure after the terminal checkpoint does not roll completion back; it
recovery-fences the audit gap. New runs also write a durable RunStart marker so
replay never confuses a partial first turn with a marker-free legacy trace.

### Work package 3 — one verification basis

Introduce an explicit verification-basis revision or digest distinct from the
whole-record CAS revision. Authoritative goal, constraint and accepted
criterion changes move the basis; progress/open-loop/next-action maintenance
may advance CAS without moving it. First define criterion origin and authority:
user/host criteria may be authoritative, while model-derived criteria remain
proposals.

Bind verification facts and all consumers to the same basis tuple. One shared
predicate must drive ActiveTurn projection, task resume, completion acceptance,
exact/domain reuse and `CompletionOpportunity`. Positive mutation evidence and
the offered-opportunity key must carry that basis; persisting a new offered key
accrues checkpoint debt so once-per-basis survives recovery.

### Work package 4 — evaluator truth reconstruction

The evaluator must derive summaries from immutable per-cell facts, never from
the control-flow path by which a cell exited:

- create harness-owned oracle directories before injection; distinguish setup,
  spawn and assertion results, and reserve `behavior=fail` for an oracle that
  actually ran;
- run the workspace self-check before oracle injection, or through a target
  proven to exclude the injected oracle, and remove the oracle before any later
  self-check; retain separate argv/results so the self-check cannot execute the
  harness oracle accidentally;
- classify provider and Runtime failures from typed sources at the point of
  failure; never infer health from a transport-error substring;
- preserve phase counters and error classes on success, cancel, timeout and
  failure; record `restored`, `continued` and `completed` independently;
- correlate interruption only with the latest exact
  `(lineage, task, sequence, artifact, checksum, capability_generation)`
  acknowledgement;
- make overall PASS the conjunction selected by an explicit persisted
  acceptance profile: behavior/diff/continuation/health plus
  `runtime_error == none` are mandatory, while closure is mandatory only for
  closure-requiring profiles and remains report-only for M15 V1;
- bind exact verification to a complete bounded workspace input snapshot,
  including all relevant source and test files;
- count opportunity call-through only from one non-empty identical key across
  Offered -> Called -> Completed, with the disabled arm emitting comparable
  explicit zeros; and
- rebuild report ratios, rounds, tool calls, prompt/schema tokens and tails
  mechanically from per-cell records. Missing measurements remain explicit,
  not zero; for two observations report both values (and any midpoint used)
  instead of labeling the upper value as the median.

Sequence-contiguity checks are per Runtime run/segment; a subscriber that starts
after the run or a merge across run ids must not manufacture a product failure.

Historical status 2026-08-28: the M15-facing reconstruction slice landed as
`retry-pilot-cell-v3`. Cells persist the acceptance profile, observed
oracle result, verdict, actual pack identity/digest, typed failure class and
independent restore/exact-tuple/continuation/turn/task facts. Runtime exposes
provider transport, model output-limit, model, input-budget, round-budget and
Runtime classes; provider retry progress is event-visible and resets the
bounded harness watchdog. Provider transport and harness failures are
NOT_RUN, while `max_output_tokens` is a model-output-limit FAIL. A window
manifest names the exact claimed cells and `REPORT.md` is regenerated from
those persisted facts with identity, terminal-event, event-contiguity,
summary/dimension and verdict consistency checks. Evidence writes fail the
cell runner instead of degrading to warnings. The formal command rejects a
dirty tree, pack subsets, repeat drift and automatic protocol negotiation.
The three retained v2 M15 attempts predate these guarantees and are
forensic-only. The three historical valid-FAIL formal windows remain v3. The
prospective schema is `retry-pilot-cell-v4`, adding stable pair/source identity,
acceptance-declaration identity and bounded request-audit facts; no v4 formal
window was banked at that point.

Repository-audit correction (2026-08-31; updated 2026-09-03): five v4
valid-FAIL windows are banked. The reporter reconstructs dimensions and
verdicts from content-addressed events/hidden verification/workspace records
and binds window manifests to cell digests rather than paths
(`M15-RAW-EVIDENCE-01`, `ea821bb`); workspace hashing shares the bounded,
link-safe allowed-diff domain, oracle timeouts kill/reap their cargo tree, and
harness failures are failure-monotone (`M15-HARNESS-BOUNDARY-01`, `f57a118`).
Historical FAIL bundles remain immutable diagnostics; the pre-window
implementation exits (formal-path retry observer `7e02488`, M10 re-audit
record, `GOV-STATUS-01` on `bba1c76`), the PinAI product preflight and the
latest 6/12 valid FAIL are recorded in [`STATUS.md`](STATUS.md).

Serving preflight status 2026-08-28: a source-bound dirty-tree diagnostic
`retry_policy_dev` normal cell passed the stricter closure-required profile on
PinAI `/v1` + `gpt-5.6-luna` + Responses + 128,000 context. It completed in 26
rounds / 59 calls / 3 failed outputs / 315,468 ms with zero provider retries.
This pinned the serving tuple for the next clean formal window at that point; it does not enter
the 12-cell result. The prior cell used 30 rounds / 53 calls / 7 failures and
did not close. The samples are too few and tool calls moved in the opposite
direction, so they prove readiness and expose failure chains, not a causal
efficiency gain.

The passing trace kept the Context hypothesis falsifiable: cumulative
historical-context prompt tokens were 8,146 while TurnFrame tokens were
119,912. Its missing-parent recovery alone required `fs.write` refusal,
loading `shell.exec`, directory creation, and a write retry across three new
model decisions. `TOOL-DIR-01` has since landed as the explicit transactional
`fs.mkdir`: one final component under an existing pinned parent, with
authority-v3 object identity and conservative rollback/reopen recovery.
`fs.write` still never creates topology. The tool is currently catalog-cold;
the `TOOL-DIR-SURFACE-01` deterministic gate (2026-08-28) proved the
failure-triggered recovery source works — a typed missing-parent refusal
surfaces exactly `fs.mkdir` with `RecoverySurface` provenance for one
decision. Its 24-cell live run did not promote the candidate, but the
post-run audit found zero `RecoverySurface`/`next_directory` exposure in all
24 traces; all eight policy cells catalog-loaded and successfully used
`fs.mkdir`. The run is therefore `NOT_EXERCISED`, not causal evidence that the
candidate increased rounds/calls. Keep the baseline and switch off
conservatively. The same audit found that `retry_diag_dev`'s checked-in golden
solution fails its own overflow oracle and that fixture self-check never runs
that oracle. That evaluator-validity gap is closed (calibrated 2026-08-29):
the golden solution saturates via `u128` widening, the hidden check demands an
overflow-safe marker, the directive names the saturate-not-wrap edge, and
fixture self-check now runs both pack oracles offline against the seed
(reject) and scripted solution (accept); the diag pack digest is regenerated
(`2fff5157…eeb`). The old
preflight pins the serving tuple but predates this product catalog revision.

### Work package 5 — execution order and live decision

Implementation and proof order is fixed by dependency, not by round budget:

1. snapshot sequence/fence and final-state consistency;
2. shared capture/validation/generation/bounds path;
3. verification basis and basis-bound opportunity persistence;
4. evaluator fixtures and report recomputation;
5. deterministic end-to-end cold-resume matrix; then
6. one decision-grade paired live gate.

Status: the deterministic cold-resume matrix landed 2026-08-28 inside the
scripted gate itself. Phase B no longer receives a phase-one checkpoint
object: the harness keeps only the acknowledged tuple
(artifact, checksum, sequence, capability generation), cold-loads the
artifact through the verified store path, cross-checks digest, sequence and
generation against the tuple, validates the payload, and only then restores
the fresh instance and continues the directive. After completion the gate
repeats the same cold chain for the TERMINAL artifact into a third fresh
instance and proves the completed task plane is visible there. Capability
generation is handshake-verified at capture, embedded in the persisted
payload and published on every `CheckpointDurable`. Retention enforces a
newest-window count and an aggregate byte budget without ever dropping the
latest artifact; the offered-key state rides the serialized resume state and
cannot re-arm on the same basis after restore.

The frozen retained-C CompletionOpportunity off/on normal/resume gate ran on
2026-08-28 with eight immutable cells and failed promotion: the off arm could
close normally and no on cell improved closure. The candidate therefore ended
default-off. This is a terminal decision for that mechanism, not a reason to
rerun it, add prompt pressure, special-case the provider or proceed to a
same-model A/C claim.

## Pre-M15 readiness task: Task-aware Completion Convergence

**2026-08-30 correction.** The 2026-08-29 implementation record below is
historical, not the current algorithm. Review of the exact source found that
settlement and `task.complete` use different completion predicates,
continuation advances the directive revision, acceptance coverage fans one
pre-success verifier identity over every criterion, trusted PASS clears
unrelated failures, and `agent-replay`'s Context rebuild admits events after the
committed turn barrier. This is not a claim that checkpoint-based production
restore already resurrects that suffix.
The reported off/on experiment also changed all of TaskProgress and checked-
file GC projection. Preserve its mechanical FAIL, but treat settlement
causality as `INVALID/CONFOUNDED`.

### Current algorithm: one dynamic readiness join

Readiness is derived from current evidence; it is not a stored boolean, token
threshold, fixed-round deadline or prompt instruction:

```text
TaskStateBasis =
  (task_id, anchor.revision)

TaskStateCurrent =
  an active task exists and matches task_id
  AND the completion intent/authority is valid
  AND execution.anchor_revision matches anchor.revision

VerificationBasis =
  (task_id, anchor.verification_revision,
   directive_revision, workspace_revision)

CommitSafe =
  TaskStateCurrent
  AND no in-flight/cancel cleanup
  AND no actor recovery fence
  AND no unresolved effect transaction / ACK debt

EvidenceReady =
  trusted verification PASS satisfies the declared identity strength
    (ExactCurrentWorld for the current development profile)
  AND that PASS is current on VerificationBasis
  AND no unresolved execution obligation
  AND no unresolved matching or unrelated blocking failure
  AND no hard required-context miss

AcceptanceReady =
  CompletionPolicy is EvidenceRequired
  AND a non-empty bounded acceptance declaration exists
  AND every declared criterion has a successful current receipt
  whose host-declared coverage domain satisfies that criterion

VerifiedReady =
  EvidenceReady
  AND AcceptanceReady
  AND open_loops is empty
  AND next_action is empty

CompletionDecision(ModelProposal) = CommitSafe AND VerifiedReady
CompletionDecision(ExplicitOperator) = CommitSafe
  # if VerifiedReady is false, persist OperatorOverride + bounded reasons
```

One Runtime-owned pure derivation accepts
`CompletionIntent = ModelProposal | ExplicitOperator` and supplies the
settlement label, optional TaskProgress projection, `task.complete` acceptance
and durable completion commit. Settlement and model `task.complete` use
`ModelProposal`. Explicit user/host closure may bypass semantic acceptance but
never runtime commit safety; it records a typed override and unmet reasons, not
verified success. Ordinary assistant final is an independent turn boundary and
never fabricates durable task closure.

Task creation or a trusted anchor-ingestion path sets an explicit completion
policy. `EvidenceRequired` carries a non-empty user/host-owned criterion set;
the conservative no-criteria default is `OperatorClosureOnly`, where ordinary
final remains valid, model `task.complete` is refused, and explicit user/host
closure is recorded as an override rather than verification success. The
model-routable `task.manage` cannot mint acceptance authority.

For V1, a criterion identity is
`(anchor.verification_revision, criterion_index)`. Its receipt also binds task,
directive/workspace revisions and verification identity. Criteria content or
order changes advance `verification_revision`; receipt mutation advances only
the full anchor CAS revision. After a successful matching observation, Runtime
atomically matches the host-declared domain, CAS-writes receipts, synchronizes
`execution.anchor_revision`, appends the bounded coverage event, and only then
derives readiness. This avoids immediately staling the PASS that earned the
receipt. Runtime never guesses equivalence from command text, and a TaskScoped
PASS cannot masquerade as `ExactCurrentWorld`.

The eval host derives declarations from each frozen fixture's public behavior
contract and persists that source digest. A retry saturation boundary may be a
criterion; a hidden implementation marker may not. A broad Cargo PASS is useful
evidence but cannot cover that boundary without the matching test/probe domain.

Continuation reuses the stored instruction and therefore preserves the
directive revision. New user dialogue advances it. A later mutation, failure,
anchor-boundary change or hard context degradation still invalidates readiness.
Failure resolution is domain/identity/resource/obligation scoped; a verifier
PASS never clears unrelated blockers.

### Current evaluation algorithm

The corrected product baseline keeps TaskProgress on and settlement projection
off. Independent switches now express that pair:

```text
project_task_progress = true
project_settlement    = false | true
```

Treatment-sized common packing and the exact same-state request audit are
implemented behind an explicit diagnostic switch. Runtime attaches an audit
only for a genuinely settled final request; the eval transport validates it
against the harness-owned arm and the exact observed messages/tools. Missing,
truncated or mismatched request evidence fails closed. The ordinary product
path packs only its actual arm and does not assemble, clone or hash a second
`ModelInput`.

This does not authorize a live causal claim: current off/on cells still start
independently. If a selected candidate enables settlement, its evaluator slice
must fork both arms from one pre-exposure durable Runtime checkpoint and
byte-identical workspace snapshot, preserve opaque ids, and pin the provider
protocol explicitly. Alpha-normalizing independently minted ids is forbidden.
A settlement-off base does not run this slice.

A settlement episode starts on entry to a ready candidate and closes on the
first reopening, durable completion, ordinary-final `TurnCompleted`, new user
boundary or continuation boundary. Persist the terminal mechanism. Pair cells
by stable `(candidate/source, pack/fixture, mode, repeat, acceptance-domain
revision/source, provider-config)` identity; runtime task ids are provenance
only. Missing, duplicate or mismatched pairs invalidate the gate. With two
repeats, report both values and their midpoint rather than hiding them behind
an upper-nearest “median”.

The real actor order places `TurnCompleted + RuntimeCommitBarrier(Turn)` before
the optional task-completion transaction. Episode parsing therefore keeps the
turn terminal pending and upgrades it only after the matching
`RuntimeCommitBarrier(TaskCompletion)`; a bounded quiet/trace boundary otherwise
closes it as an ordinary turn. Deterministic coverage uses that order.

### Current delivery order

The latest formal window (`_windows/1788385151733`, 2026-09-03) is a valid FAIL
6/12 with 0 NOT_RUN on clean source `43e1033` and the PinAI/Luna tuple. Migrate
passed 4/4; diag passed 1/4; policy passed 1/4. The immutable streams show three
separate failure surfaces: the overflow-edge behavior miss, long completion-
gate tails, and one fail-closed bounded-framer malformed-event. Continue in this
order:

1. Preserve the window and do not rerun its exact source/serving candidate.
2. Reconstruct each failure from typed events, verification and final workspace
   truth; do not merge the three surfaces without evidence.
3. Select one bounded execution/provider candidate from that diagnosis. Keep
   Context selection, GC, retrieval, packing and transcript history unchanged.
4. Pass deterministic failure-path regressions and the applicable open P1 exits
   or exact selected-path exclusions, then record the complete local/dual-CI
   source gate.
5. Run the same-checkpoint causal fork only for a settlement-enabled candidate.
6. Run one fresh exact-source product preflight and at most one freshly
   predeclared formal window if every prior gate remains green.

#### 2026-09-03 workload split

The proposed one-stop reliability path is **large overall** and is recorded here
before implementation. It is at least five independently reviewable slices;
the size labels below are relative implementation plus deterministic-test
costs and exclude a live M15 window.

| Slice | Size | Boundary and disposition |
| --- | --- | --- |
| Eval CLI parses every option before side effects, including position-independent `--evidence-dir` | S | Supporting fix only; it does not change the rejected candidate or any historical verdict. |
| Hermetic developer/eval doctor and gate runner (real Python/tool paths, owned helper binaries, exact Provider data-plane probe, unique evidence output, local/CI parity) | L | `EVAL-PREFLIGHT-01`; orchestration of existing commands and manifests only, never a second Runtime authority. |
| General `AttemptIncident` versus completion-debt admission | L | Requires trusted dispatch/effect/task-root attribution and checkpoint-compatible negative tests; never infer safety from argv, prose or exit code alone. This is a possible product candidate, not yet selected. |
| Broader completion convergence beyond the existing sole-proof refresh | L | Highest authority risk. Any candidate stays inside the single `RuntimeActor`, reuses `CompletionReadiness`, requires explicit intent and the same basis, and may resolve only a host-proved mechanical blocker. No automatic debt/progress clearing or task completion. |
| Buffered stream normalization and local-cap taxonomy | M | Preserve independent byte/chunk/wire bounds. First prove coalescing and attribution without accepting a runaway stream or weakening fail-closed behavior. Not currently required to explain the valid FAIL. |
| Public behavior/property verifier with a separate hidden holdout | M code / L governance | Changes fixture/acceptance identity and task difficulty. Parked unless an explicit prospective M15 refreeze selects it; never rewrite prior cells. |

The umbrella therefore cannot be implemented or evaluated as one patch. A
pre-M15 decision chooses one bounded product/serving candidate and gives it its
own deterministic matrix. Supporting gate-runner work may proceed separately,
but formal preflight and window remain two explicit operator-reviewed steps;
the runner must never chain them automatically. Any readiness receipt is a
derived check over the existing content-addressed manifests and digests, not a
second source of evidence authority.

No step expands the transcript, adds a second ResumePoint, introduces a
TaskGraph/learned planner, auto-completes, fixes a task-specific round count or
retunes Context/GC/retrieval/packing. Executable defect exits are centralized in
[`AUDIT_TODO.md`](AUDIT_TODO.md#current-merged-audit--2026-08-30-ea8deef).

### Historical 2026-08-29 implementation record (non-actionable)

Do not start by rewriting `task.complete`. In the recovery-surface run its
schema was always present, all 18 calls returned successful tool results, and
17 reached durable `TaskCompleted`; the 55-round / 129-call tail made no
completion call. The bottleneck is the decision before a completion mechanism,
not the mechanism itself.

#### Audit of the landed first slice

CONV-CLOSE-01 landed useful workspace-cleanliness alignment, bounded
`SettlementLabel` events, first-candidate metrics and seven deterministic actor
scenarios. Its live run also produced 4/4 passing cells with event exposure.
Those facts remain valid, but review found four gaps:

1. `TaskProgressView.settlement` is not rendered by `PromptAssembler`; the
   model did not receive the claimed one-line decision fact.
2. `ExecutionState::settlement()` joins only verification validity and the
   execution-obligation ledger. It does not see current task/user authority,
   acceptance coverage, `TaskAnchor.open_loops`/`next_action`, known failures
   or actor in-flight state. Its `SettledCandidate` is execution-local, not a
   whole-task readiness result.
3. `post_settlement_profile` counts everything after the first candidate even
   after a later mutation returns the state to `Working`; legitimate phase-two
   development is therefore charged as convergence tail.
4. `--conv-gate` runs normal/resume with both existing switches off. It has no
   settlement-projection control arm, so its 4/4 result proves observation
   exposure and task success, not causal round/call reduction.

The evidence report remains immutable; only its causal interpretation is
superseded. No further live spend is justified until the four gaps are fixed.

#### Superseded target algorithm

Use a two-level, evidence-driven join rather than a fixed round or token limit:

```text
ExecutionReady
  = current trusted verification covers
      (task verification revision, directive revision, workspace revision)
    AND no in-flight/cancel cleanup
    AND no unresolved execution obligation or failed command

TaskReady
  = ExecutionReady
    AND current user/task epoch still matches
    AND open_loops is empty
    AND next_action is empty
    AND every bounded acceptance criterion has current explicit evidence

Working | VerificationDue | VerifiedCurrent | SettledCandidate
                                          ^ only when TaskReady
```

With no task-level acceptance coverage, the strongest truthful state is
`VerifiedCurrent`. Acceptance coverage is criterion-addressed and linked to a
bounded evidence identity; free-form “done” text cannot establish it. Reuse the
existing TaskAnchor CAS, verification tuple and evidence refs where possible;
add only the smallest typed coverage primitive if the existing types cannot
express the join. A new directive/constraint, anchor-boundary change, accepted
mutation, failure, stale verification, opened loop or non-empty next action
reopens the state immediately. No fixed threshold decides readiness.

#### Superseded delivery and promotion gate

1. **Correct semantics first.** Keep settlement model projection absent and
   default-off. Add deterministic negative coverage for incomplete acceptance,
   open loop, next action, failed command, new directive, boundary change,
   in-flight cleanup, mutation-after-verify and cold restore. Preserve ordinary
   final, durable closure and genuine remaining-work positives.
2. **Bounded projection second.** Wire one neutral fact through the
   Runtime-owned `PromptAssembler` only after step 1 is green. Model-request
   tests must prove presence only for `TaskReady`, absence for every reopening
   case, and the existing 2,048-character TASK PROGRESS bound. The fact offers
   ordinary final, durable `task.complete`, or concrete continuation; it is not
   a stop instruction, auto-close or capability lease.
3. **Episode accounting.** A settlement episode begins on entry to a
   task-aware candidate and ends at the first reopening transition or terminal
   outcome. Report its rounds/calls/failures and whole-cell totals. Never attach
   later legitimate work to an earlier episode.
4. **Real paired gate.** Add a default-off projection switch. For each repeat,
   run the same pack/source/serving in projection-off and projection-on arms;
   normal and resume appear in both arms, with at least two paired repeats.
   Zero on-arm exposure is inconclusive. Promotion requires mandatory
   behavior/diff/resume parity, no lost unfinished work, lower candidate-episode
   rounds and calls, and no new maximum episode or whole-cell tail.
5. **Historical M15 handoff (superseded).** This slice required a promoted
   projection before exact-source preflight. The current rule allows a corrected
   settlement-off base candidate; only an enabled projection needs its own
   causal gate. In either case, do not repeatedly sample an unchanged candidate.

Steps 1–4 landed 2026-08-29 as a bounded batch. Live candidate emission
required three driver repairs first: trusted verification PASS clears
identity-exact `failed_commands` on the current basis/directive/workspace
tuple (fresh failures re-block), request-level (~62 s) plus whole-cell
(3 runs, 30 s/60 s backoff) provider retry, and a live acceptance data
source — the harness patches the bounded acceptance declaration and the
runtime binds the current trusted verification pass as the coverage claim
for every declared criterion at observation time.

The step-4 paired gate then ran on the approved 8-cell budget
(`--allow-dirty`; `project_progress` is the only arm difference): 8/8 cells
PASS behavior/diff/closure/continuation, 0 NOT_RUN — but the verdict is
FAIL. Pair 0 (normal r1) has settlement exposure off=none / on=seen (the
off cell recorded no trusted verification pass: all four `verify.run` calls
used the TaskScoped `rust.workspace` runner, which executes but carries no
exact identity, so the join never armed and the zero-exposure cell is
inconclusive by rule; exposed cells used the host-registered
`jobrunner.exact` recipe, whose pass arms the candidate synchronously with
its observation), marker-violation counts differ in 3/4 pairs (needle-shape
misses the harness oracle tolerates, present in both arms), and
episode-rounds/calls medians are 1→1, not strictly lower. Projection
rendering was real and arm-separated: off 0 tokens every round, on 430–512
tokens once a candidate existed. The frozen rule therefore keeps the
projection default-off and returns the gate to observation: no rerun before
a bounded diagnosis of the recipe-choice exposure and marker-parity causes,
and no promotion claim. Facts:
[`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md).

Context selection, GC, retrieval and prompt packing stay frozen. The prior C
Context advantage remains banked: this task targets execution amplification
without expanding the transcript, adding a second ResumePoint, or weakening
model autonomy.

## Post-M15 next phase task: `LT-EVAL-06` — representative development twins

Do not add another planner first. After formal M15, evaluate the frozen
`LT-RUN-05` Runtime on longer but still oracle-checkable development work:

1. a diagnosis-and-fix task whose defect location is not named by the prompt;
2. a bounded multi-file API migration with compile/tests and an allowed-diff
   oracle; and
3. an evaluation-harness maintenance task that changes code, adds or repairs a
   test, executes it, and produces a mechanically reviewable result.

Each task has normal and cold-resume twins and at least two repeats under one
frozen source, serving tuple, tool surface and acceptance profile. The resume
arm receives only the acknowledged lineage/task/sequence/artifact/checksum/
capability-generation tuple. Harness setup, behavior, diff, verification,
closure, provider health, Runtime health, restore and continuation remain
independent facts. Failed tool outputs stay in the denominator.

The deterministic side of task 3 landed 2026-08-29 as `harness_maint_dev`
(frozen digest `c586021e…a2d91`, registered in `m15_pack`): a
`summarize_results` seed whose `NotRun` rows are counted as failed with all
green tests, an injected harness-owned oracle rejecting the seed and accepting
the scripted minimal fix, content checks (report names the responsible
function and mechanism; `failed` counts only `Failed` rows; the regression
test locks `total == passed + failed + not_run`), and self-tests pinning
seed-reject / solved-accept / oracle / digest. It is a deterministic
LT-EVAL-06 fixture, not an M15 window pack — `M15_PACK_IDS` stays 3.

Use the current bounded `TaskRecord.resume: ExecutionState` as the only
progress substrate. Its content follows actual goal/constraint/evidence
changes and coalesced safe points; do not prescribe fixed subtask counts,
checkpoint intervals or transcript windows. Report lost constraints,
duplicated reads/effects, time and decisions to first new post-resume outcome,
rounds, calls, failure classes, schema/prompt tokens and historical Context
tokens. Preserve the existing Context/GC/prompt policy.

Only evidence recurring across independent task families may open a TaskGraph,
CPL/cycle advisory or richer planning slice. A single slow cell, provider
failure or evaluator defect cannot. If normal/resume twins retain mandatory
success, exact-once effects and bounded recovery without material repeated
work, the correct result is to keep the simpler substrate and expand task
breadth rather than add control state.

### Non-goals

- no transcript expansion or second task/turn authority;
- no Context selection, GC, retrieval or prompt-packing retune;
- no fixed checkpoint interval, fixed subtask count or score-driven stopping;
- no model-visible TaskGraph, CPL authority, learned router or semantic planner;
- no provider/model special case and no standing “stop earlier” instruction;
- no new live run before the deterministic truth chain is green.

### `LT-RUN-05` verification and exit gate

Minimum deterministic coverage:

- behavior PASS + missing closure records behavior PASS, closure FAIL and
  overall FAIL; closure PASS + behavior FAIL also remains overall FAIL;
- provider/runtime error, failed restore or failed continuation makes overall
  FAIL even when behavior, diff and closure happen to pass;
- oracle setup/start failures produce typed `not_run`, while an executed
  assertion failure produces behavior FAIL; the external oracle is independent
  of agent-editable tests, the workspace self-check cannot execute the injected
  oracle, and neither argv/result can overwrite the other;
- typed provider/Runtime failures remain non-healthy regardless of error text;
- success, timeout, cancellation and error exits all preserve their actual
  per-phase rounds, tool calls, tokens and outcome fields;
- two different snapshots under one task-anchor revision receive different
  sequences, and a task switch cannot move snapshot order backwards;
- an old acknowledgement arriving after a new mutation cannot clear newer debt
  or release continuation; an in-flight write that accrues debt is followed by
  another capture;
- write failure at sequence zero and at a repeated task-anchor revision blocks
  continuation until the exact required sequence succeeds;
- a loaded capability and all other Runtime planes round-trip under a stable
  capability generation through one validated, bounded persisted artifact;
- corruption, truncation, checksum mismatch, oversized input and stale
  capability generation refuse before restored state mutation;
- phase two uses a distinct Runtime and Context engine and receives only the
  exact acknowledged lineage/task tuple, never a phase-one checkpoint
  object/reference;
- event identities within one durable lineage prove
  `TaskResumeCommitted(S)` ->
  `CheckpointDurable(S, artifact, digest, capability_generation)` ->
  `TaskContinuationStarted(S)` with no mismatched/older ack accepted;
- the final completion artifact is validated, loaded and restored successfully
  in a fresh Runtime before the test accepts
  `TurnCompleted + RuntimeCommitBarrier(Turn)` -> prospective terminal
  `CheckpointDurable` -> `TaskCompleted +
  RuntimeCommitBarrier(TaskCompletion)`; final-write failure leaves the live
  task completion-pending/retryable and emits no `TaskCompleted`, while a
  post-checkpoint audit failure keeps terminal truth and fences recovery;
- a stress fixture creates `configured_retention_limit + 2` checkpoints and
  crosses the configured byte threshold; count/bytes remain bounded without
  deleting the latest required, pinned or referenced recovery artifact;
- progress-only record movement preserves verification across every consumer,
  while authoritative basis movement invalidates it across every consumer;
- criterion, receipt and verification provenance all bind the same current host
  coverage-declaration revision/source digest; legacy/default or recomposed
  mismatches fail closed;
- any settlement-changing causal pair forks from one pre-exposure durable
  checkpoint and byte-identical workspace, preserves opaque ids and uses an
  explicit provider protocol; and
- initial/incomplete tasks never receive a derived completion lease; current
  positive evidence offers one bounded decision; Offered -> Called -> Completed
  carries one non-empty identical key, which survives cold restore and cannot
  re-offer without a new valid basis; and
- no duplicate committed effect appears after restore.

The CompletionOpportunity off/on promotion gate was then executed. All
mandatory dimensions were present, but the candidate failed the frozen
promotion rule and ended default-off. Therefore it does not enter same-model
A/C and must not be rerun as an unbounded optimization search. Diagnosis and
migration breadth moves to post-M15 `LT-EVAL-06`; TaskGraph/Completion shadow
evidence remains non-authoritative.

## Metrics and gate

Primary, counted over every assigned cell:

- harness-owned behavioral/API/README/allowed-diff outcomes, workspace
  self-check, closure and provider/runtime outcomes reported independently;
- task identity/constraints survive restore;
- no repeated committed effect after continuation;
- required verification is Current at successful completion;
- no `Unknown` or recovery fence is hidden;
- normal and resumed twins reach equivalent accepted behavior;
- acknowledged full-plane lineage/task/sequence/artifact/digest/capability-
  generation tuple and distinct restored Runtime/Context identities;
- explicit setup, provider, runtime, restore and continuation error classes,
  with no missing value silently interpreted as zero or healthy.

Efficiency is secondary and reported per solved task:

- model rounds, tool calls, max-turn tail and wall time;
- provider input/output, historical Context, selected tokens and resident
  bytes;
- evidence-only calls, repeated reads, catalog-control calls and failed tool
  outputs by class;
- completion opportunities eligible/offered/called/accepted/refused, offers
  per unique key, decisions from offer to closure and added prompt/schema
  tokens;
- progress-only versus verification-basis changes, invalidations by cause and
  verifier reruns after progress-only changes;
- recovery overhead after restore: repeated calls, reread motive and time to
  first new outcome;
- required/durable snapshot sequences, checkpoint bytes, capture/sync/restore
  latency, coalesced debt, write-in-flight state, checksum/fence/write failures,
  generation retries and restored capability count.

Safe edit refusals, verifier failures, provider failures and filesystem
settlement failures remain separate classes. None is silently removed from
the denominator. A product candidate requires harness-owned behavioral parity
first; only then may lower rounds/calls support an efficiency claim.

## Exit from this phase

Exit only when:

1. every deterministic gate above is green;
2. every acknowledged checkpoint in the deterministic matrix validates and
   restores into fresh Runtime/Context instances, including the final
   completion artifact; retention count/bytes remain bounded under stress
   without deleting the latest required, pinned or referenced artifact;
3. exact snapshot identity and the no-debt/no-failure/no-in-flight fence hold
   under same-anchor snapshots, out-of-order acknowledgements, task switches,
   new debt during write and failed-write retry;
4. progress-only changes preserve verification while semantic-basis changes
   invalidate it consistently across ActiveTurn, resume, exact/domain reuse,
   completion and `CompletionOpportunity`;
5. one unchanged completion key cannot produce repeated hints or leases across
   a cold restore, and old mutation evidence cannot arm a new basis;
6. evaluator fixtures prove typed oracle setup/start/assertion outcomes, full
   PASS conjunction, complete failed-path accounting and exact resume-tuple
   correlation;
7. evidence and the summary report rebuild mechanically from immutable
   manifest/events/oracle/workspace facts with no arithmetic discrepancy;
8. the default-off candidate's deterministic replay and eight-cell live gate
   are reconstructed mechanically; the failed promotion is retained as the
   terminal decision and the candidate stays off;
9. no committed effect is duplicated and no recovery/unknown state is hidden;
10. no implementation replays transcript history, changes Context/GC/prompt
    policy, fixes progress to a predetermined size or special-cases the
    provider/model in product Runtime policy.

The rejected promotion does not authorize same-model A/C cells or a new
Context-efficiency claim. `LT-RUN-05` closes as a durability/evaluator repair,
not as an execution-efficiency promotion. Formal M15 still uses its separately
frozen acceptance design; `LT-EVAL-06` is the post-M15 breadth step.
