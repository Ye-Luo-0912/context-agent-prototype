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

## Runtime slices to implement before the live development pilot

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
No live claim yet; the safe-point and completion slices below remain
open before the pilot.

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
- Explicit pause, suspend, completion and clean shutdown wait for the durable
  checkpoint acknowledgement. A background checkpoint failure keeps the debt
  visible and retryable; it never reports the task as safely resumable.
- The checkpoint contains existing Runtime planes and the authority-WAL
  marker. It does not serialize raw transcript history or an in-flight
  prepared effect.
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
re-arms the debt and emits `CheckpointWriteFailed` instead of claiming
resumability. Completion and continuation wait for in-flight writes.
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
or network. Hidden checks validate boundary values, overflow saturation,
permanent-error behavior, public API compatibility, README update and the
allowed workspace diff. Multiple correct implementations are accepted; a
golden text patch is not the oracle.

Two modes share the exact seed and directive:

- `normal`: uninterrupted end-to-end execution;
- `resume`: after the first durably settled workspace mutation creates
  checkpoint debt, the harness waits for its checkpoint, stops the runtime,
  constructs a new runtime, restores, and calls `continue_active_task`.

The interruption trigger is a semantic event, not a fixed round number.

## Evaluation layers

Do not vary model, Runtime and tool surface in one comparison.

1. **Deterministic runtime gate:** scripted decisions cover progress CAS,
   safe-point ordering, checkpoint failure/retry, stop/restore/continue,
   no duplicated effect commit and completion refusal with stale verify.
2. **C live development pilot:** `normal` and `resume`, two repeats each,
   using one pinned model/provider. These four cells validate the harness;
   they are not acceptance.
3. **Agent/runtime comparison:** same pinned model, A/C context engines,
   normal/resume, two repeats (eight paired cells). This isolates Runtime and
   Context behavior.
4. **Model comparison:** freeze the retained C runtime and tool surface, then
   vary models on the same tasks. This measures model tool-use quality, not a
   Context algorithm change.
5. **Pack expansion:** only after the first task is green, add one diagnosis
   task and one multi-file migration task. Do not jump from one task to the
   frozen 300x3 M15 gate.

The cell counts above are experimental design. Runtime termination remains
state-based: verified acceptance, resolved required blockers and explicit
completion. Round/call/token/time values are safety ceilings, never target
step counts or automatic declarations of success.

## Metrics and gate

Primary, counted over every assigned cell:

- hidden build/tests and allowed-diff oracle pass;
- task identity/constraints survive restore;
- no repeated committed effect after continuation;
- required verification is Current at successful completion;
- no `Unknown` or recovery fence is hidden;
- normal and resumed twins reach equivalent accepted behavior.

Efficiency is secondary and reported per solved task:

- model rounds, tool calls, max-turn tail and wall time;
- provider input/output, historical Context, selected tokens and resident
  bytes;
- evidence-only calls, repeated reads, catalog-control calls and failed tool
  outputs by class;
- recovery overhead after restore: repeated calls, reread motive and time to
  first new outcome;
- checkpoint bytes, write latency, coalesced debt count and restore latency.

Safe edit refusals, verifier failures, provider failures and filesystem
settlement failures remain separate classes. None is silently removed from
the denominator. A product candidate requires hidden-success parity first;
only then may lower rounds/calls support an efficiency claim.

## Exit from this phase

The long-task slice is ready to join broader M15 work only when deterministic
runtime coverage is green, every first-task normal/resume live cell passes,
the evidence bundle rebuilds from manifest/events/verify/workspace facts, and
no design requires raw transcript replay. This remains development evidence;
formal M15 still requires its separately frozen acceptance design.
