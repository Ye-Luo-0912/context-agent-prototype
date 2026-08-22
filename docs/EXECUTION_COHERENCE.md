# Execution Coherence V1

RC (release candidate). Code: `crates/agent-runtime/src/execution/`.
The 2026-08-21 blockers — MOD-OBS-01 (refusal-as-observation),
MOD-PROG-01 (progress / stall), turn checkpointing — landed; freeze
re-designation waits for the next live evidence pass (production-surface
convergence), not for new code here. Checkpoint field is still `resume`
on `TaskRecord` so old snapshots load.
Do not add a second task table and do not reimplement `ResumePoint`.

Context packing, GC, and retrieval live in
[`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md). Current freeze / P0 live in
[`STATUS.md`](STATUS.md).

## Invariants

1. **Unknown ≠ False, and NeedsRevalidation ≠ Fresh.**
   Do not delete facts to hide uncertainty. An unknown process write bumps
   `workspace_revision` (old PASS is omitted) and marks known
   `path@revision` rows `NeedsRevalidation`. Runtime re-hashes; it does
   not pretend the files are unchanged or gone.

2. **ObligationExists ≠ DueNow.**
   A verification obligation (source change, spec change, failed
   verification) persists across user turns. PreferSurface Verify *this
   round* only when `verification_due_now` is true. Do not wipe a real
   obligation just to avoid showing Verify, and do not infer NeedVerify
   from nonempty `acceptance_criteria` or from a later note turn.

3. **IdentityKnown ≠ BodyVisible.**
   A `path@revision` in TaskProgress, a selected descriptor, or an
   EXTERNAL CONTEXT ref is not a file body. Checked omit, descriptor
   packing, and Foreground Evidence exist because of this gap. The model
   may still `fs.read`; Runtime must not treat identity as content.

4. **One Model Round = One Execution Snapshot.**
   After BeforeModel revalidate, capture one [`RoundExecutionSnapshot`].
   Prompt, `ContextHints`, and tool-surface policy all read that snapshot.
   Do not clone `ExecutionState` and replay `TurnFrame` per consumer.
   Durable `TaskRecord.resume` is installed only after `TurnCompleted`.
   Cancel / fail / stale drops the ephemeral turn projection.

There is no planner. The LLM still chooses actions. Runtime only
maintains what the world can currently prove.

## Ownership

```text
Task authority            → Runtime / TaskAnchor
Operational state         → ExecutionState
Historical selected       → ContextEngine
Exact old evidence        → Catalog / Search / Fetch
Body residency            → GC
Prompt                    → Runtime assembler
```

Prompt framing is `TASK ORIGIN` (historical `original_goal`),
`PERSISTENT TASK STATE` (constraints / acceptance / plan / open loops),
and `CURRENT DIRECTIVE` (this user turn). Temporal wording on the origin
is not a perpetual instruction. A new user turn replaces TurnIntent; it
does not rewrite TaskSpec, bump `anchor_revision`, or wipe the
verification ledger.

## Phases

```text
World Facts (path@rev / errors)
  → Freshness Engine (Fresh / NeedsRevalidation / Missing)
  → Obligation Ledger (verify / failure / unresolved evidence)
  → Round Projection (due_now / foreground refs / missing evidence)
       → Prompt and Tool roles → LLM
```

### ResourceFact

Bounded `path` + SHA-256 digest + `ResourceFreshness` + turn + diagnostic
`provenance` (`Read` / `MutationResult` / `MutationRefusal` /
`Verification`). Cap 32. Stamped from trusted `ResourceTouch`
(`metadata.path` and `metadata.files[]`). Successful `fs.write` /
`edit.replace` / `edit.patch` heat from those touches. A stamped
`shell.exec` path is identity, not a new file body.

**MOD-OBS-01 — a refused mutation is still an observation.** Effect,
observation, and attention are separate truths. A deterministic edit
refusal (`stale_revision` / `no_exact_match` / `ambiguous_match`) read
the target to refuse it, so its trusted `path`+`revision` stamp upserts
a Fresh fact with provenance `MutationRefusal` — this resolves
`NeedsRevalidation` without a model-driven re-read and updates the
digest honestly (a real drift marks the source changed). A failed
*read* saw nothing and stays out of the fact table. Attention is
unchanged: failed outputs never heat the working set.

**Commit-time content conflict.** Existing edit targets are leased by
canonical path key before one bounded transaction snapshot is taken; batch
keys are sorted, and every prepared child retains the shared lease group
through final composite settlement. Same-`Workspace` writers therefore
queue and re-snapshot the settled winner while unrelated paths remain
parallel. A prepared mutation re-checks the target immediately before
replace. Drift from an external writer already visible when that check begins
settles as definite `NotApplied` with `stale_revision`; it is not a durability
failure and does not require the recovery fence. Hash then rename is still
not an atomic CAS against another process or independently opened workspace
authority. Runtime drops the prepared output's proposed
`revision` / `files[]` metadata from every result other than fully durable
`Applied`, retains only bounded `attempted_paths`, and attaches the trusted
failure class where non-application is definite. Because a receipt does not
carry an intervening writer's body/revision or the exact subset of a partial
composite, Runtime does not manufacture Fresh resource facts from it.

### Freshness

`may_mutate_workspace` is the authority fence. Knowledge uses
`MutationFootprint` (`None` / `Known(touches)` / `Unknown`).

- Known touches upsert Fresh facts.
- A deterministic refusal (`ToolFailureClass::nothing_executed()`) is
  `NotApplied`: footprint `None`, no `workspace_revision` bump, no fact
  staled (MOD-OBS-01). Process/timeout/cancellation/io failures keep
  the conservative `Unknown` bump.
- Unknown marks known facts NeedsRevalidation and may set
  `unknown_pending` on the obligation. It must not `checked_files.clear()`.
- BeforeModel revalidates up to 8 pending facts through
  `ResourceVersionOracle` (hash only; no body in the prompt; no extra
  model round).
- TaskProgress `Checked` and body-omission consume only **Fresh**
  identities.

### Verification

Persistent facts: obligation (`cause`, `coverage`,
`required_for_completion`) and evidence (`result`, `anchor_revision`,
`workspace_revision`, optional `evidence_ref`).

`validity()` is Current / Pending / Stale / Failed / NotRun. After an
Unknown mutation, `Coverage::Resources` may recover Current when every
covered path revalidates identity-unchanged; `Workspace` and
`Unspecified` cannot.

NeedVerify prefers a declared `Verify` role, else catalog discovery, else
`EscapeHatch`. `InspectDiff` is not a verifier. Runtime does not encode
that cargo uses `shell.exec`. No builtin currently declares `Verify`. Do
not add `verify.run(command)`. Natural-language verify is a frozen
four-needle hint, not a dictionary.

Completion consumes the obligation when `required_for_completion` is
true (default false: do not force a test run on every task).

### RoundExecutionSnapshot

Fields: `progress`, `foreground_resources`, `verification`, `needs`.
`RoundNeeds` fact-gaps are `VerificationDue`, `UnresolvedFailure`,
`EvidenceNeeded`, and `OpenLoopNeedsEvidence`. Runtime PreferSurfaces
`context.manage` for evidence gaps. It does not PreferSurface
Read / Search / Mutate as an action plan. `NeedMutate` is not inferred
from a nonempty instruction.

Each user turn seeds ephemeral `ActiveTurn.execution` from
`TaskRecord.resume`. Persistable tool results `observe_tool` immediately.
Durable resume waits for the turn barrier.

### Foreground evidence

When CURRENT DIRECTIVE exactly names an ExecutionState-known path
(path-token match), Runtime fills `ContextHints.foreground_resources`
(max 2). The engine may copy the latest matching body into
`MaterializedContext.foreground` for this request only: Warm stays Warm,
Stored is not Admitted. Engine packing rules are in
[`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md).

### Progress / stall detection (MOD-PROG-01)

Every persistable tool result gets one deterministic classification in
`observe_tool`: `Meaningful` (world changed) > `Evidence` (new/updated
fact or verification evidence; a repeated identical verification row is
not evidence) > `Control` (a failure row cleared) > `None`. There is no
planner — the runtime only states what the world can prove changed.

A consecutive `None` counter per operation signature
(tool + target + failure class) resets on any progress or signature
change. At 3 consecutive no-progress rounds, `TaskProgressView`
carries a bounded `EXECUTION STALL` advisory line (rendered under the
TASK PROGRESS header, never trimmed away). It is advice, not an
execution block: the model still chooses.

**Deterministic duplicate refusal.** An identical retry of a refused
`edit.replace` / `edit.patch` — same argument digest, and every refusal
target `path@digest` still Fresh and unchanged — is provably
deterministic. The actor refuses it before admission with failure class
`duplicate_no_progress` (`nothing_executed`, counts toward the stall).
The attempt ledger is turn-scoped (`ActiveTurn`, cap 8): a new user
directive may legitimately ask for the same edit, and facts not Fresh
fail open to dispatch. Process/shell tools are never deduplicated —
time and environment make them non-deterministic.

### Protocol working set (turn checkpointing)

The current turn is itself a working set. The wire view of `TurnFrame`
keeps only the last `TURN_FRAME_KEEP_EXCHANGES` (6) completed tool
exchanges; older ones compact to a bounded deterministic
`TURN CHECKPOINT` note injected right after the user directive. Whole
call+result groups are dropped so the wire protocol keeps every tool
call paired with its result; the trailing (possibly in-flight) region is
always retained. The runtime's full frame is never mutated — audit,
`ToolFinished` events, and turn-end persistence still see every step.
`ModelInput.turn_checkpoint` records the compacted count; token
accounting (`PromptLayerCosts.turn_frame_tokens`) measures the wire
view. No LLM summary is involved anywhere: the checkpoint is a pointer
to TASK PROGRESS / artifacts / the run journal, where the compacted
exchanges' reliable facts already live.

## ObservationMemo

Unwired: `lookup()` is always a miss. First wired version, when allowed,
is `fs.read` keyed by path + line range + content revision. Never memoize
writes, patches, or shell. Memo cannot replace this algorithm (the model
round has already happened).

## Lifecycle clocks and maintenance scheduling

The runtime keeps exactly four logical clocks, and nothing else may
advance time:

| Clock | Advances on | Used for |
| --- | --- | --- |
| `event_seq` | any real state change | audit / ordering / stamps |
| user turn | a committed user input | Context TTL / semantic aging |
| model round | each inference request | Tool Surface lifecycle |
| `gc_epoch` | each full Context GC pass | residency generations |

Invariants: loading a tool does not make time pass; executing a tool does
not age the user turn; materializing never advances a lifecycle clock;
and a no-op never consumes `event_seq`.

**Tool surface clock (landed).** `BuiltinToolDispatcher` and
`CapabilityRegistry` previously advanced their private tick on load,
execute, *and* every gc() safe point, so `idle_to_warm_ticks = 8`
could elapse within 2–3 real rounds of a tool-heavy trajectory and cool
in-use tools off the surface (measured live: 20 forced
`capability.manage op=load` calls in one 15-turn cell, zero of them
redundant). Now loads/executes only stamp `last_used_tick` with the
current value; the clock advances **only** inside `gc()`, which the
runtime calls once per model round. Idle thresholds are therefore model
rounds, and both halves stay in lockstep because their gc() runs from
the same safe point.

**Exactly-once tool-scope closes (landed).** Round prep used to rescan
every historical `TurnFrameStep::ToolResult` each round — O(R²), with a
lock/acall per id even when already closed, and
`ContextEngine::close_scope` bumped `event_seq` for those no-ops.
`ActiveTurn.pending_scope_closes` now enqueues each scope once when its
result settles; round boundaries drain it; cancellation drains it plus
the in-flight op's scope. The engine bumps `event_seq` only when a close
actually produced transitions.

**Maintenance debt gate (landed).** `BeforeModel` used to run the full
minor scan every round (77x/cell measured) even with zero pending state
changes. The engine now stamps `last_maintained_seq` when a pass
completes; a `BeforeModel` trigger at an unchanged sequence returns a
default report — no scan, no `event_seq` bump, so idle rounds stop
consuming sequence space. Lifecycle-closure triggers always run: they
carry semantics beyond dirty work. Bounded dirty batches instead of full
heap scans stay a later step.

Queued (design agreed, not yet landed):

- **Convergence failure clusters**: after ≥2 consecutive same-class
  failures over an unchanged world revision (e.g. invented-program
  PathNotFound streaks), surface an `EXECUTION STALL` line built from
  the recorded cluster instead of letting the model guess another
  spelling.
- **Protocol evidence instrument**: add a reread motive class
  (`protocol_checkpoint_body_missing`) before building any body cache;
  implement the tiny current-turn LRU only if that motive shows up.

## What not to add

Typed EpisodeOutcome, a smarter reactivation scorer, embeddings, RAG, a
learned router, a new GC generation algorithm, or a third operational
table. Do not retune `active_threshold` / `archive_threshold` /
`gc_max_generation`.

## Evaluation

`add_test` is Tool Surface (`historical_context=0`), not this contract.
Context live `context-mech.v2` (A/C × 3 tasks × 2 repeats = 12 cells) is
in `crates/agent-eval/evidence/context-mech/REPORT.md`. Do not expand to
27 cells or 300×3. Do not use one A/B/C for Tool Surface, Context, and
Effect Runtime. Evidence lives under
`crates/agent-eval/evidence/*/REPORT.md`.
