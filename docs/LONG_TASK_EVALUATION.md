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

Today a full `RuntimeCheckpoint` is externally requested and an active turn's
final `ExecutionState` becomes durable only after `TurnCompleted`. Add an
actor-owned safe-point continuation path; do not let the TUI/evaluator become
a second orchestrator.

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
  artifact is actor-owned and deliberately omits the host capability plane;
  closing that ownership seam is part of `LT-RUN-04` below. Neither form may
  serialize raw transcript history or an in-flight prepared effect.
- Restore reuses the existing validation/fence. A new
  `continue_active_task` runtime command starts a fresh `ActiveTurn` from the
  same task id, current directive, anchor and resume state; it does not mint a
  user instruction or increment the directive identity.

The implementation must add explicit events for progress update, safe-point
resume commit, durable checkpoint acknowledgement/failure and continuation
start. Eval must be able to prove their order from JSONL.

Status: landed deterministically 2026-08-25. Accrued debt reasons are task
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
read-only no-debt, store atomicity, and continuation identity.

### LT-RUN-03 — completion and recovery gate

Keep the landed final-answer/task-closure split. A claimed successful
completion is accepted only after the sibling batch settled, every required
obligation is resolved, required verification is Current, and no effect plane
is `Unknown`/`RecoveryRequired`. Runtime attaches its own assistant and
verification refs. Partial/failed completion may retain explicit open loops;
success cannot silently erase them.

The final durable checkpoint is taken after `TurnCompleted` and before the
task scope is reported safely closed. This slice extends existing completion
tests; it does not make `task.complete` permanently visible.

Status: landed deterministically 2026-08-25. The acceptance gate re-runs at
every completion edge: no recovery fence, no unsettled cancelled operation,
zero open failure obligations, required verification current, and no open
loops silently erased. A gated proposal on the one-shot path returns the
decision to the model with one warning per turn instead of committing;
the deferred and `/done` paths fail their transaction with the typed
reason. Success orders `TurnCompleted` -> final `CheckpointDurable` ->
`TaskCompleted`, so JSONL proves closure was durably checkpointed before
it was reported. A failed final write surfaces as a warning that never
un-completes the task and never claims resumability. Deterministic
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
self-check; adding an external oracle is part of `LT-RUN-04`. Multiple correct
implementations must be accepted; a golden text patch is not the oracle.

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
   Status: green for `retry_policy_dev` as of 2026-08-25. The scripted
   normal/resume gate (`agent-eval --long-task-gate`, also run as a cargo
   test) drives two real runtime instances over the production tool
   surface — progress CAS at the operation commit, safe-point ordering,
   stop/restore/continue through the shared durable authority lineage,
   exactly-once byte-exact effects, deterministic marker acceptance and
   `TurnCompleted` -> final durable -> `TaskCompleted` ordering.
   Directive tools are leased from their catalog-cold state through
   `capability.manage`, exactly as a live run must. Checkpoint
   failure/retry and completion-refusal-with-stale-verify remain covered
   by the LT-RUN runtime slices. The first layer-2 C cells have since run;
   layers 3+ remain open.
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
3. **Completion-candidate isolation:** after `LT-RUN-04` deterministic gates,
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

## Next phase task: `LT-RUN-04` — trustworthy closure and cold continuation

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
- `task.complete` stays catalog-cold during ordinary work. No automatic task
  completion and no natural-language classifier that guesses whether a final
  answer means whole-task closure.
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

The final cell verdict remains the conjunction required by the existing gate;
separating dimensions does not turn partial work into PASS. It prevents a
closure failure from erasing whether the implementation was behaviorally
correct. Provider, verifier, safe edit, filesystem settlement and lifecycle
failures remain different classes.

Run read-only diff and behavioral acceptance whenever the final workspace is
inspectable, including after missing closure or a provider failure that occurs
after edits. `not_run(reason)` is reserved for an unavailable workspace or an
oracle that cannot start, not for failure in another dimension. Report the
workspace's own `cargo test` separately as an agent-authored self-check. Add a
harness-owned, network-free external test crate (or equivalent isolated
oracle) for retry bounds, transient/permanent behavior, delay saturation and
overflow, and public API compatibility; keep README semantics and allowed diff
as harness-owned checks. Do not use implementation markers or a golden patch
as behavioral acceptance.

For resume cells, phase two must construct a fresh Context implementation and
load the persisted checkpoint artifact. Reusing the phase-one engine object is
not a cold-resume result.

Status: landed 2026-08-25. The evaluator records the six outcome dimensions
independently and read-only acceptance always runs while the workspace is
inspectable; a harness-owned frozen-public-API oracle is injected after the
run and executed in isolation from the agent-editable workspace, and the
agent's own cargo run stays a separate non-gating self-check. Resume twins
build a fresh Context engine per phase and cross the boundary holding only
the acknowledged artifact locator, which is checksum-verified on load.
No live cell has yet passed the full conjunction.

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

The current model-routable `task.manage` cannot edit acceptance criteria, but
the trusted `AnchorPatch` API still classifies them as autonomous and a newly
created task normally has no typed criteria. Before any criterion-level hard
gate, define criterion origin/authority and how a user directive is ingested
into stable criterion identities. Until then, acceptance-to-proof work is
shadow-only research.

Acceptance for this slice:

- plan/open-loop/next-action-only updates preserve Current verification;
- goal/constraint/authoritative-criterion changes make dependent verification
  stale;
- whole-record stale CAS still refuses without mutation;
- checkpoint/restore preserves both revisions and their binding;
- ActiveTurn, task resume, exact reuse and completion agree on currentness;
- completion records remain bound to the authoritative task basis.

Status: landed deterministically 2026-08-25, as authority-gated staleness on
the existing single record revision rather than a second counter: the whole
anchor CAS still advances one monotonic revision and the resume fence always
follows it, but only boundary-class movement (goal/constraints) marks a
Current verifier stale with cause `SpecChanged`; progress-only CAS keeps the
verifier Current while stale-base writes stay refused. The behavioral
acceptance bullets above are covered by deterministic TaskManager tests in
both the patch and whole-anchor paths. A dedicated verification-basis counter
stays open and is not needed by any current caller; if criterion-level gates
arrive they must first define criterion origin/authority as stated above.

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

Status: landed behind a default-off host switch
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
executed offer -> leased call -> committed closure -> all dimensions pass —
but paired outcomes fell (2/4 versus 1/4), medians rose, arming stayed rare
(2 of 6 on-cells), and one journal-lock flake censored an off cell.
Decision-grade reruns require a higher arming rate and that lock fix first;
upgrading discovered general runners remains a separate gated decision.

### Slice D — one complete durable checkpoint boundary

The current actor safe-point artifact contains the actor-owned checkpoint
plane and an empty host-capability list. A full `RuntimeCheckpoint` is produced
only when `RuntimeInstance::checkpoint()` mechanically merges the host registry
under its generation handshake. The pilot disabled capability-aware mode, so
this omission is not an explanation for its missing closure; it is a production
recovery-contract gap. `LT-RUN-04` must make the safe segment boundary use the
same complete ownership model. Centralize external and automatic
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

Replace reason-vector inference as durability truth with monotonic watermarks
equivalent to:

```text
resume_state_revision
required_durable_revision
durable_revision
```

Debt reasons remain useful observability. Continuation across a segment is
allowed only when `durable_revision >= required_durable_revision`. A write,
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

Status: landed deterministically 2026-08-25. Safe-point persistence runs
through one full-plane coordinator that captures the host capability plane at
spawn-injected registry handles, and `CheckpointStore` writes a header +
payload envelope with sha256 and an OS sync barrier before the atomic rename,
with a verified load path refusing truncation or corruption. Continuation is
gated by monotonic watermarks (`resume_state_revision`,
`required_durable_revision`, `durable_revision`) and fails closed with a typed
fence until a retried write lands; turn-end settlement flushes accrued debt so
a failed write retries even without a tool batch. `CheckpointDurable` carries
the acknowledged revision and checksum.

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

### Verification and exit gate

Minimum deterministic coverage:

- behavior PASS + missing closure records behavior PASS, closure FAIL and
  overall FAIL; closure PASS + behavior FAIL also remains overall FAIL;
- a provider failure after edits still runs the oracle when the workspace is
  inspectable; only a real workspace/oracle-start failure records `not_run`;
- the harness-owned external oracle is independent of agent-editable tests;
- initial and incomplete tasks never receive a derived completion lease;
- Current trusted terminal evidence offers `task.complete` for one bounded
  decision; one key offers once, ignoring it never auto-completes, and
  invalidation retracts it;
- progress-only anchor updates do not stale verification while authority
  changes do; ActiveTurn, resume, exact reuse and completion agree;
- a loaded capability and all other Runtime planes round-trip through one
  persisted artifact;
- corruption, truncation or checksum mismatch refuses before state mutation;
- phase two uses a distinct Context engine and no phase-one in-memory
  checkpoint or engine reference;
- checkpoint failure/checksum mismatch blocks continuation, retry success
  releases it;
- no duplicate committed effect appears after restore;
- event revisions prove `TaskResumeCommitted(N)` ->
  `CheckpointDurable(N, artifact, digest)` ->
  `TaskContinuationStarted(N)`;
- a successful final-checkpoint path retains `TurnCompleted` ->
  `CheckpointDurable` -> `TaskCompleted`; final-write failure remains a
  separate visible outcome and never claims resumability.

Then execute the CompletionOpportunity off/on promotion gate above. All outcome
dimensions must be present; its four candidate-on cells must pass behavioral,
diff, lifecycle and normal/resume-equivalence gates, and the paired efficiency
and tail criteria must pass before the candidate can become the frozen default.
Only that promoted setting may enter same-model A/C. Afterward the pack may add
diagnosis and migration tasks or collect Completion/TaskGraph shadow evidence.

## Metrics and gate

Primary, counted over every assigned cell:

- harness-owned behavioral/API/README/allowed-diff outcomes, workspace
  self-check, closure and provider/runtime outcomes reported independently;
- task identity/constraints survive restore;
- no repeated committed effect after continuation;
- required verification is Current at successful completion;
- no `Unknown` or recovery fence is hidden;
- normal and resumed twins reach equivalent accepted behavior;
- acknowledged full-plane artifact/digest/revision and distinct restored
  Context engine identity.

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
- required/durable resume revisions, checkpoint bytes, capture/sync/restore
  latency, coalesced debt, checksum/fence/write failures and restored
  capability count.

Safe edit refusals, verifier failures, provider failures and filesystem
settlement failures remain separate classes. None is silently removed from
the denominator. A product candidate requires harness-owned behavioral parity
first; only then may lower rounds/calls support an efficiency claim.

## Exit from this phase

Exit only when:

1. every deterministic gate above is green;
2. all four new C normal/resume cells run the independent oracle and pass
   behavioral, closure and applicable continuation dimensions with the
   CompletionOpportunity candidate on;
3. both resume cells load a checksum-valid full-plane artifact into a distinct
   Context engine, with no in-memory phase-one checkpoint crossing the seam;
4. no committed effect is duplicated and no recovery/unknown state is hidden;
5. every `TaskCompleted` follows explicit authorized closure intent — model
   `task.complete` or a trusted `/done`/host path; successful final-checkpoint
   ordering is proven, while failure remains visible and makes no resumability
   claim;
6. the default-off candidate passes the deterministic replay and at least two
   C off/on paired repeats per normal/resume mode, including behavior, outcome,
   median round/call and tail gates;
7. one unchanged completion key cannot produce repeated hints or leases;
8. progress-only changes preserve verification while semantic-basis changes
   invalidate it consistently across ActiveTurn, resume, reuse and completion;
9. evidence rebuilds from immutable manifest/events/oracle/workspace facts;
10. no implementation replays transcript history, changes Context/GC policy or
   special-cases the provider/model in product Runtime policy.

Only then may same-model A/C cells make a new Context-efficiency claim. This
remains development evidence; formal M15 still requires its separately frozen
acceptance design.
