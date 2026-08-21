# Execution Coherence V1

Freeze candidate. Code: `crates/agent-runtime/src/execution/`.
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

Bounded `path` + SHA-256 digest + `ResourceFreshness` + turn. Cap 32.
Stamped from trusted `ResourceTouch` (`metadata.path` and
`metadata.files[]`). Successful `fs.write` / `edit.replace` / `edit.patch`
heat from those touches. A stamped `shell.exec` path is identity, not a
new file body.

### Freshness

`may_mutate_workspace` is the authority fence. Knowledge uses
`MutationFootprint` (`None` / `Known(touches)` / `Unknown`).

- Known touches upsert Fresh facts.
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

## ObservationMemo

Unwired: `lookup()` is always a miss. First wired version, when allowed,
is `fs.read` keyed by path + line range + content revision. Never memoize
writes, patches, or shell. Memo cannot replace this algorithm (the model
round has already happened).

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
