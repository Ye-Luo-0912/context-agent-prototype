# Execution Coherence V1

**Freeze candidate** (2026-08-23). Code: `crates/agent-runtime/src/execution/`.
The 2026-08-21 blockers — MOD-OBS-01 (refusal-as-observation),
MOD-PROG-01 (progress / stall), turn checkpointing — landed, and the
clean post-outage A/C longflow pass (n=2, all four arm-runs hidden-pass)
held the machinery: Warm=Stored rereads stayed 0 and capability churn
stayed gone. Do not retune or extend this layer as product work.
Checkpoint field is still `resume` on `TaskRecord` so old snapshots load.
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

5. **Global frontier advance ≠ blocker resolution.**
   Reading a new file, learning a status, or seeing a new error are all
   real frontier advances, yet none of them moves the current blocker.
   Convergence is therefore two-layered: the Global Evidence Frontier
   answers "did the agent recently gain new world facts at all" (metrics,
   stall advisory, evidence working set); the Obligation Ledger answers
   "has this known blocker's own precondition changed". Unrelated
   progress must never clear an obligation.

6. **Evidence stored ≠ evidence still valid.**
   A row in the evidence table is a claim with a validity binding
   (world revision / resource digest / turn), not a durable truth. One
   shared `evidence_is_current` predicate governs both projection and
   sweeping; restore may not assume a checkpoint was written by trusted
   code, so converged state carries bounds and validation like any other
   restored input.

7. **Raw evidence ≠ Runtime authority.**
   Producer-stamped output metadata (path/revision/verification/mutation
   flags/failure class) is model-facing payload. Runtime-trusted facts
   come from operator-builtins, effect receipts, workspace handles and
   the process host — or from nothing. Dynamic capability outputs are
   sanitized at the routing layer and contribute no authority facts
   (CAP-OBS-01); the typed host-trusted facts channel is the follow-up
   mainline before Self-Iteration.

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

**Coherence vs convergence.** The guarantees above are coherence: the
runtime states what the world can prove and never blocks on guesses.
They do not by themselves establish that the *task* moved forward.
The convergence extension below landed 2026-08-23; it stays advisory
only and never becomes a planner.

### Execution Evidence Frontier (CONV-01, landed)

Every persistable tool observation is classified into one deterministic
[`FrontierDelta`]: `ObservedWorldChange` (Known footprint applied),
`WorldInvalidatedUnknown` (Unknown footprint — knowledge may be stale,
but this is **not** progress), `EvidenceAdvanced`, `ObligationResolved`,
`RedundantEvidence`, `NoProgress`. Only the first, third, and fourth
clear the stall signature, the failure cluster, and
`actions_since_frontier_advance`; a redundant re-read cannot launder an
active failure cluster. Successful read-only observations
(`git.status`/`git.diff` keyed by tool at `WorkspaceRevision(n)`;
path-carrying reads keyed `tool:path` at `Resource{path@digest}`) enter
a bounded evidence table (≤16 rows, newest-first); a world-revision
advance expires revision-bound rows and counts as
`evidence_invalidations`. Each round emits one bounded
`ExecutionFrontier` event; eval aggregates `frontier_advances`,
`redundant_evidence_calls`, `frontier_no_advance_peak`,
`evidence_invalidations` from the event stream alone. After 5
non-advance actions the TASK PROGRESS view renders a soft advisory:
"EXECUTION FRONTIER UNCHANGED …". TASK PROGRESS carries typed fields
only (identities / enums / digests / counts); raw bodies stay in the
user-role layer or artifacts.

### RetryDomain (CONV-02, landed)

`FailureClass` ("what broke") and failure domain ("which preconditions
does a retry depend on") are separate vocabularies. Hard refusal is only
legal under provable precondition equivalence: the edit duplicate guard
(argument digest + all target identities Fresh), plus
`ExecutableResolution` for process/shell launches — same argument digest
(argv0/cwd/env overrides) with no workspace-revision advance since the
failure; the ledger's precondition is the host-trusted
`resolution_fingerprint` (cwd listing + PATH + env), so a rebuilt binary
changes the precondition while an unrelated source edit does not.
Timeout, exit codes, cancellations stay `NonDeterministic` and
are never refused hard; cross-name guessing loops are suppressed by the
soft convergence debt above. There is deliberately **no K-strikes name
ban**: the cwd listing is bounded, PATH/extensions/later builds can
change any conclusion.

### Protocol body cache (PROTO-EVID-01/02, landed)

A per-turn LRU (≤4 entries, ≤8 KiB each, `ActiveTurn` lifetime) keeps
recently observed file bodies from successful `fs.read` results only —
an edit echo is a patch echo, not the exact body, and never enters the
cache. A Known mutation invalidates its touched paths; an Unknown
mutation invalidates everything. During assembly a body is re-injected
as a **user-role context-frame message** (never the Focus frame, which
renders as System policy — PROMPT-AUTH-01) only when the turn checkpoint
actually truncated that read, the TASK PROGRESS fact is still Fresh, and
the digest is identical. Cache rows never enter the context engine, are
never admitted, and never persist. Every assembly emits one bounded
`ProtocolBodyCacheStats` event (eligible / hit / miss / invalidated /
oversize / restored_body_tokens), so hit-rate claims are verifiable from
the event stream alone.

### Obligation Ledger (CONV-03, landed)

The frontier's global counters cannot see blocker debt: interleaved
unrelated advances keep `frontier_no_advance_peak` under threshold while
a guessing loop burns rounds. The ledger is per-blocker: a failed output
whose failure domain is typed (`ExecutableResolution`, `EditTarget`,
`ResourcePath`, `ProjectMarker`) opens an `ExecutionObligation` keyed by
domain + precondition fingerprint. For process launch failures the
runtime stamps a host-trusted `resolution_fingerprint` (cwd listing +
PATH + env overrides) into the failure metadata — source edits do not
change it, builds do. Rules: unrelated progress never resolves an
obligation; resolution requires the same domain to succeed or the
recorded precondition to demonstrably change (new target digest, known
mutation touching the path); same domain with a different precondition
supersedes the old row; `NonDeterministic` domains open nothing. TASK
PROGRESS renders at most two bounded UNRESOLVED BLOCKER lines beside the
global advisory. Evidence argument identity uses the Runtime-computed
`ArgumentDigest` (not producer strings), so same-argv/different-env and
same-path/different-cursor calls no longer collide on evidence identity.

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
view. No LLM summary is involved anywhere: the checkpoint is safe only
where a compacted exchange's facts are already projected as typed
operational evidence (Execution Frontier, CONV-01) or its raw body is
still reachable by reference / the current-turn protocol cache; the
journal is an audit backing store and does not by itself make facts
model-visible.

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

**Reread motive attribution (landed).** The E2E motive axis gained
`protocol-checkpoint-body-missing`: an identity-only frame for a body the
model consumed earlier (read-provenance fact, unchanged digest,
descriptor residency). It splits that population out of
`descriptor-only`/`needs-revalidation` so any protocol evidence body
cache would be sized against measured demand. The tiny current-turn LRU
is implemented only if the motive shows up in live runs.

**Convergence failure clusters (landed).** The per-signature stall
counter cannot see invented-path streaks that vary the spelling every
attempt. `ExecutionState` therefore also aggregates consecutive
same-class failures across different targets over an unchanged world:
at ≥2 distinct targets, `TaskProgressView.stall_warning` carries an
EXECUTION STALL line naming tool and class, built from the recorded
cluster. Mixed failure classes and any progress restart it; the
identical-signature threshold stays 3. Advisory only. Replacing the
full Resident+Warm heap scan in `run_minor` with bounded dirty batches
(`MaintenanceDebt`) remains queued behind idle-round evidence.

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
