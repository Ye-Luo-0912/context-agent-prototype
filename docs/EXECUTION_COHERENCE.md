# Execution Coherence V1

**Freeze candidate** (2026-08-23). Code: `crates/agent-runtime/src/execution/`.
The 2026-08-21 blockers — MOD-OBS-01 (refusal-as-observation),
MOD-PROG-01 (progress / stall), turn checkpointing — landed, and the
clean post-outage A/C longflow pass (n=2, all four arm-runs hidden-pass)
held the machinery: Warm=Stored rereads stayed 0 and capability churn
stayed gone. A 2026-08-24 diagnostic tested a model-visible cross-turn
task-change provenance projection and rejected it: a refinement amplified
constraint turns to 127 rounds / 174 tool calls in C. That field is not part of
this contract. A generic current-workspace-authority standing prompt was also
rejected after two repeats amplified unrelated completion/verification turns.
Do not otherwise retune or extend this layer as product work.
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
   EXTERNAL CONTEXT ref is not a file body. A historical body may collapse
   to a descriptor only when `ContextHints.visible_body_identities` proves
   that the retained turn tail or checkpoint restoration carries the exact
   same `path@revision` body in this request. The model may still `fs.read`;
   Runtime must not treat identity as content.

4. **One Model Round = One Execution Snapshot.**
   After BeforeModel revalidate, capture one [`RoundExecutionSnapshot`].
   Prompt, `ContextHints`, and tool-surface policy all read that snapshot.
   Do not clone `ExecutionState` and replay `TurnFrame` per consumer.
   Durable `TaskRecord.resume` is installed only after `TurnCompleted`.
   Cancel / fail / stale drops the ephemeral turn projection.

5. **Evidence storage ≠ task-frontier advance ≠ blocker resolution.**
   Reading a new file, learning a status, or seeing a new error can all add
   real evidence, yet may move neither the current directive nor its blocker.
   Convergence is therefore layered: the Evidence Frontier answers “did the
   agent gain a task-relevant current fact” (metrics, stall advisory, evidence
   working set); once a directive already has an exact Fresh task root, novel
   unrooted observations stay stored but do not clear convergence debt.
   Directives without an exact root retain broad exploration. The Obligation
   Ledger answers
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
   (CAP-OBS-01). `ToolDispatcher::execution_attribution` is now the bounded
   pre-dispatch channel for purpose, resource identities and verification
   reuse policy; Runtime separately joins those identities with current task
   roots. Unattributed capabilities and generic shell/process fail closed.

8. **PreconditionChanged ≠ ObligationResolved.**
   A blocker obligation is a lineage: stable scope identity, per-epoch
   precondition fingerprint, attempts in epoch, total attempts. World
   movement (a build landing, PATH changing) advances the epoch and
   gives the model room to retry once more; it never clears the debt.
   Only blocker-specific proof resolves an obligation — for
   ExecutableResolution, a successful launch carrying the same scope
   key *and* fingerprint.

9. **CachedBytesPresent ≠ BodyCurrentlyTrusted.**
   Unknown-footprint mutations suspend cached protocol bodies (bytes
   retained, eligibility frozen) instead of deleting them. Eligibility
   returns only when BeforeModel revalidation proves the identical
   path@digest Fresh again; a changed identity can never pass the gate.
   Known-footprint mutations still physically drop their touched paths.

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
  model round). Exact bodies about to spill from the checkpoint consume
  that existing quota first, followed by verification coverage, directive
  mentions, and recency; the quota does not grow.
- TaskProgress `Checked` consumes only **Fresh** identities. Body omission
  additionally requires exact same-request body presence; Fresh identity
  alone is insufficient.

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
that cargo uses `shell.exec`. When the trusted composition root discovers or
installs at least one bounded project recipe, the builtin surface declares
`verify.run { recipe_id }`; argv/cwd/env are absent from model arguments and
come from the same immutable recipe set that builds Core's `HostEffectBinding`.
Do not add `verify.run(command)` or parse a shell string into verification.
Natural-language verify is a frozen four-needle hint, not a dictionary.

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
but this is **not** progress), `EvidenceAdvanced`, `EvidenceReconfirmed`,
`ObligationResolved`, `RedundantEvidence`, `NoProgress`.
`EvidenceReconfirmed` means the same argument + semantic outcome became
current again after invalidation; it is observable but is not new knowledge.
Only `ObservedWorldChange`, task-relevant `EvidenceAdvanced`, and
`ObligationResolved`
clear the stall signature, the failure cluster, and
`actions_since_frontier_advance`; a redundant re-read cannot launder an
active failure cluster. Successful read-only observations
(`git.status`/`git.diff` keyed by tool at `WorkspaceRevision(n)`;
path-carrying reads keyed `tool:path` at `Resource{path@digest}`) enter
a bounded evidence table (≤16 rows, newest-first). Invalidated rows remain
non-current in that same bounded table only as semantic fingerprints and
never project into TaskProgress; a matching result can therefore repair
currentness without being counted as `EvidenceAdvanced`. Each round emits
one bounded
`ExecutionFrontier` event; eval aggregates `frontier_advances`,
`redundant_evidence_calls`, `reconfirmed_evidence_calls`,
`frontier_no_advance_peak`,
`evidence_invalidations` from the event stream alone. After 5
non-advance actions the TASK PROGRESS view renders a soft advisory:
"EXECUTION FRONTIER UNCHANGED …". TASK PROGRESS carries typed fields
only (identities / enums / digests / counts); raw bodies stay in the
user-role layer or artifacts.

Task relevance is derived from the same trusted host attribution plus Runtime
task authority used by execution-policy checks. On a new directive,
`path_exactly_in_directive` intersected with Fresh checked resources activates
the targeted mode; a later successful rooted observation can activate it too.
New unrooted evidence is still recorded with its exact validity binding, but
is classified `NoProgress` for the advisory frontier. No call is refused, no
search budget is reduced, and no free-text plan chooses the next action.

### Outcome shadow and optional-surface attribution (CONV-OBS-02, measurement landed)

The Evidence Frontier above answers whether Runtime learned a new current
fact. It does **not** answer whether the task result moved. A different path,
search argument, Git view, or directory listing can be honest new evidence
while leaving every task obligation unchanged. Therefore the evidence clock
must remain intact for coherence, but it must not be used alone to justify
optional capability residency or a convergence claim.

`agent-eval` now derives a second, decision-free shadow from existing bounded
events. It reports three orthogonal facts for `ToolFinished` results:

- successful Known mutation results and host-stamped typed verification
  results (outcome advances; the two labels may overlap but one result advances
  once);
- Unknown-footprint invalidations (a typed verification can also be Unknown);
- evidence/control results without either outcome or Unknown invalidation.

`TaskCompleted` is also an outcome advance. The aggregator resets its
per-directive streak on applied `UserMessageAccepted`, counts every
`ToolFinished` including `TransientNoPersist`, and reports
`max_results_without_outcome_advance`. This deliberately does not add a second
Runtime planner, a TaskProgress row, a Context item, or transcript history.

The same aggregator joins `ToolSurfacePlanned.selected` provenance to the
following requests and reports catalog-loaded optional exposure, schema tokens,
requested calls, unused rows, rounds without a request, and report truncation.
The names say `reported`: selected rows are event-bounded, so an old or
truncated report never pretends to be a complete surface.

The retained long-flow reaggregation is causal support, not a tuning sample:
C/A had the same eight Known mutation outcomes, but 48/21 evidence-only
results, 9/0 Unknown invalidations, and a maximum 18/3-result outcome-free
streak. C exposed 134 optional catalog rows and requested 18; A exposed 28 and
requested two. See the source-bound report under
`crates/agent-eval/evidence/longflow-task-provenance-2026-08-24/`.

### Dynamic execution flow (exact-current PASS slice landed; broader equivalence open)

Do not turn the shadow streak into a hard `N`-call stop or infer necessity by
parsing arbitrary shell commands. A required verification can be Unknown at
the workspace-effect plane, while an irrelevant read can be perfectly new.
The decision layer is therefore host-attributed and purpose-scoped. Its first
two runtime slices landed 2026-08-24; they remain deliberately smaller than
the complete verification/provenance design:

- `ActiveTurn.result_delivery_tools` and `pending_loaded_tools` are orthogonal
  ephemeral source sets, not TTLs. A successful model decision roots every
  exact tool it calls until those results reach the following successful
  decision; reuse renews that result-delivery source. A trusted
  `capability.manage load` receipt instead adds its exact target to the pending
  set until exact use, explicit unload, or directive end. Adjacent explicit
  loads can therefore coexist, while using one consumes only that pending
  member. Both sets are turn-local and enter neither Context nor checkpoints.
- At the first model safe point of a new applied directive, and after every
  successful model decision, Runtime projects exact task requirements, typed
  verification/evidence need, pending explicit loads, active/result-delivery
  calls and the decision's exact calls into dispatcher roots. A non-empty
  decision consumes pending roots only for the exact tools it calls; an empty
  terminal decision releases unused pending members. Loaded optional schemas
  without a source make a `Loaded -> Warm` transition. Warm is off-surface but
  remains in the catalog and exactly reloadable; no permission, effect
  authority or Context state changes.
- Surface entry has an explicit source contract. A host/operator
  `load_tool` is persistent until explicit unload; Runtime task/NeedEvidence
  reloads and model `capability.manage load` use `load_tool_for_lease` and do
  not create a hidden task-global pin. Reconciliation reports host-persistent
  and runtime-root retention separately. A runtime lease reload cannot weaken
  an existing host source. Checkpoints carry mechanical residency only:
  restored-only rows remain releasable, while current-run host sources already
  established by composition are unioned with restored residency. None of
  these sources grants invoke authority.
- `ExecutionBatchSettled` accounts every model-requested result batch with
  body-free counters, including transient results, access-only results,
  no-dispatch refusals and exact duplicate reuse. `requested == terminal` and
  zero unexpected terminals are integrity gates. Accepted calls are bounded by
  the provider batch safety cap before they enter the queue/TurnFrame. An
  oversized provider batch executes no member and is terminalized as scalar
  no-dispatch refusal counts, so even the safety refusal cannot disappear from
  accounting. The ledger lives only in `ActiveTurn` and enters neither Context,
  prompt history nor checkpoints.
- `ToolLeasesReconciled` reports exact examined/retained/released totals plus a
  bounded name sample. Retention is partitioned into runtime roots and
  persistent host sources. Eval reports these together with action accounting
  and the following optional-surface exposure. The 32-call provider batch
  limit is only a queue/TurnFrame memory boundary; it is never a target, expiry
  rule or convergence stop. Because lease reconciliation mutates the next
  model surface and batch settlement is an acceptance ledger, failure to append
  either event sets `RecoveryRequired` and prevents another model decision;
  these are fail-closed evidence, not best-effort telemetry.
- `ToolDispatcher::execution_attribution(call)` now records a bounded trusted
  purpose (`Observe/Search/Mutate/Verify/Control/Opaque`), canonical resource
  identities and an explicit verification-reuse policy *before* dispatch.
  Builtins copy only known path fields; arbitrary queries, content and command
  text never enter the channel. Dynamic capability calls default to
  Unattributed even if their manifest declares a Verify role. Shell/process
  remain Opaque and producer output metadata cannot upgrade either one.
- Runtime joins those host identities with current task authority. Exact
  paths in current intent/interpretation, constraints, acceptance criteria,
  plan progress, open loops, checked facts, obligations and mutation
  preconditions are rooted; historical origin text alone is not. An unrooted
  trusted `path_not_found` becomes one of eight revision-bound negative facts,
  not an obligation. Before an equivalent read/search is skipped, the
  Workspace oracle must still return absent and the `Reused` audit event must
  append successfully; otherwise Runtime executes normally. A relevant task
  root promotes the next miss to an obligation, a live external file
  invalidates the row, and every admitted Known/Unknown mutation conservatively
  invalidates all rows. `ExecutionNegativeFact` events and `negative_fact_*`
  eval metrics expose every lifecycle transition without storing a body.
- A reusable verification result can now be minted only by pre-dispatch
  trusted Verify attribution. `TaskScoped` checkpoints exact source-tool and
  argument identity under the current task-anchor revision for later
  PreferSurface, but always dispatches a requested verification.
  `ExactCurrentWorld` additionally requires a non-empty host identity digest
  over the recipe revision plus every relevant execution-profile, policy and
  environment input not already in canonical call arguments. Only the SHA-256
  digest crosses the attribution/event/checkpoint boundary; raw environment
  material does not. Runtime recomputes this trusted attribution after the
  verifier settles; pre/post mismatch keeps the result as TaskScoped evidence
  and cannot mint an exact receipt for a mixed external world.
  Runtime stores that provenance on the existing bounded `VerificationFact`;
  it does not add a task table, Context item or transcript row.
- A PASS is reusable only for the same task state, anchor revision, user-
  directive revision, workspace revision, exact tool, Runtime
  `ArgumentDigest`, and host verification-identity digest, while derived validity is
  still Current. The directive revision makes a later user request a genuine
  rerun boundary; any admitted workspace mutation, anchor/argument/host-
  identity change or failed/stale obligation falls through to real dispatch.
  Before a skip, `ExecutionVerificationPass::Reused` must append successfully;
  otherwise the verifier runs. The model receives a truthful terminal result
  with `executed=false`, and `ExecutionBatchSettled.reused` plus
  `verification_pass_*` metrics account for the avoided dispatch. Receipt
  lifetime is identity-driven, not round/time TTL.
- The production `verify.run` entry point now consumes only a bounded
  host-registered `recipe_id`. Project discovery is bounded (recipe count,
  directory entries and manifest bytes), and the recipe table is wired once
  into both `BuiltinToolDispatcher` and `HostToolPolicyRegistry`; an unknown id
  resolves to empty process authority and fails before spawn. General
  Cargo/Go/npm/Yarn/pnpm/pytest/CTest runners are typed `TaskScoped`
  verification because their test bodies can have arbitrary side effects.
  A manifest-free standalone Rust `rustc --test <file> -o
  .focus-agent/...` compile recipe is the first automatically discovered
  `ExactCurrentWorld` verifier: output stays under ignored runtime state. Its
  identity hashes recipe revision, platform/architecture, the resolved
  executable, a bounded complete inherited environment, and the complete
  bounded workspace file set excluding `.git` and `.focus-agent`. The broader
  snapshot is required because `rustc` may load sibling `mod` files not named
  in argv. Links/reparse escapes, special files, `include*`/`#[path]`,
  unreadable entries or count/byte overflow downgrade to `TaskScoped` rather
  than truncating a validity claim. A pre/post identity mismatch is stamped on
  the terminal output and also downgrades. The schema is present/required only
  when at least one recipe exists. Generic shell/process remain Opaque and
  producer metadata cannot opt in.

The remaining decision layer stays host-attributed and purpose-scoped:

1. Extend the landed exact-current identity with obligation revision and
   broader coverage-complete relevant-input identities where workspace-wide
   revision plus the recipe's explicit inputs is too conservative. Only a
   host-registered verifier can settle it;
   model/plugin metadata and generic process/shell cannot mint a reusable PASS.
2. Join identical in-flight leases and add host-declared equivalent (not just
   exact) PASS reuse. Exact completed PASS reuse is landed; any relevant
   input/environment change already opens a new identity and dispatches.
   If complete coverage cannot be proven within the bound, widen it to
   Workspace; never truncate coverage and retain a validity claim.
3. Extend the landed result-delivery and anchor-bound verification sources
   with durable obligation/provenance sources. Core/always-loaded tools and
   explicit `TaskToolRequirement`s are already rooted; unresolved typed obligations
   must keep their exact source tool only after the obligation ledger records
   that trusted association. The model can always search and reload an
   unrooted optional tool.
4. Keep execution accounting orthogonal to Context persistence. Capability
   and context search/inspect/fetch remain transient and body-free, but their
   request/result receipts still count as execution actions.

#### Host-declared equivalence classes (designed 2026-08-26; staged implementation)

Open decision items 1–2 get this concrete shape. Equivalence between two
verifiers is always declared by the host that registers the recipes; nothing
infers it from command strings, argv or output text.

- The existing single recipe table (wired once into
  `BuiltinToolDispatcher` and `HostToolPolicyRegistry`) grows three bounded
  fields per entry plus one side table owned by the same composition wiring:
  `coverage_domain: Option<BoundedDomainId>` on every recipe, and
  `VerificationCoverageDomain { domain_id, declaration_revision }` with
  `members: Vec<(recipe_id, recipe_revision)>` declared per domain. The
  model-visible schema stays `verify.run { recipe_id }`; none of this is
  model-authorable.
- A recorded PASS fact keeps its exact tuple exactly as today. The reuse
  predicate widens from "same recipe" to: resolve the requested recipe in the
  current composition; require its domain to equal the recorded fact's
  domain, require the fact's stored `declaration_revision` to equal the
  current one (a meaning bump invalidates all older facts), require both
  recipe revisions to sit in one declared class, and then apply every
  existing ExactCurrentWorld check unchanged. Any miss dispatches for real.
- Class membership is evaluated against the *current* composition, never
  checkpointed tables; checkpoints keep carrying only per-fact revisions, so
  restore needs no new residency rules.
- The appended `ExecutionVerificationPass::Reused` event gains a bounded
  discriminator (`exact` vs `{domain_id, declaration_revision}`); eval splits
  `verification_pass_reused` accordingly. Event-append failure keeps fencing
  exactly as today.
- Fail-closed surface: unknown recipe/domain, missing class membership,
  revision mismatch or unresolvable composition → dispatch. Required
  deterministic tests before default behavior: same-domain cross-recipe
  reuse; cross-domain never reuses; declaration-revision bump invalidates;
  unknown ids dispatch; restore under a recomposed table; append-failure
  fence.

Obligation-scoped provenance sources (open item 3) follow the same pattern:
extend the lease source enum with `ObligationProvenance { obligation_id }`
retained exactly while the ledger row lives; the association is written only
by the trusted code path that records the obligation, never by producer
metadata.

Lease lifetime is never a fixed number of calls, rounds or seconds. A lease is
live exactly while its typed source set is non-empty; source identity changes
open a new epoch. Numeric limits exist only at allocation/wire boundaries
(provider call batch, task requirements, coverage representation and event
samples). Crossing a safety bound refuses or widens conservatively; it never
silently truncates an authority/validity claim. Store only ids, digests, enums,
counters and artifact refs; raw output remains stored once. The landed slices
changes neither ContextEngine policy nor effect authority. The remaining
verification-equivalence/provenance behavior must pass deterministic
already-satisfied, exact/equivalent verification, stale-settlement and restore
tests before it can become a default decision source.

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

### Protocol body cache (PROTO-EVID-01/02/03, landed)

A per-turn LRU (≤4 entries, ≤8 KiB each, `ActiveTurn` lifetime) keeps
recently observed file bodies from successful `fs.read` results only —
an edit echo is a patch echo, not the exact body, and never enters the
cache. A Known mutation physically drops its touched paths; an Unknown
mutation **suspends** every entry — bytes retained, eligibility frozen
(CachedBytesPresent ≠ BodyCurrentlyTrusted) — and BeforeModel
revalidation proving the identical path@digest Fresh revives it.
During assembly selection is checkpoint-demand driven: Runtime compares the
bounded full `ActiveTurn` with the retained six-exchange tail and selects at
most four exact spilled `fs.read` bodies (≤8 KiB each). The full frame is
already the open-turn audit backing, so an older demanded body remains
available even when a latest-read LRU retained newer rows still in the tail.
A body is re-injected
as a **user-role context-frame message** (never the Focus frame, which
renders as System policy — PROMPT-AUTH-01) only when the turn checkpoint
actually truncated that read, the TASK PROGRESS fact is still Fresh, and
the digest is identical. Cache rows never enter the context engine, are
never admitted, and never persist. `eligible` counts actual fresh checkpoint
demand, not all cache rows, so tail-resident rows do not inflate misses.
Every assembly emits one bounded `ProtocolBodyCacheStats` event
(eligible / hit / miss / invalidated /
suspended / oversize / restored_body_tokens), so hit-rate claims are
verifiable from the event stream alone.

### ProgramResolver + Obligation Ledger (TOOL-PROC-01 / CONV-03, landed)

The frontier's global counters cannot see blocker debt: interleaved
unrelated advances keep `frontier_no_advance_peak` under threshold while
a guessing loop burns rounds. The ledger is per-lineage: a failed output
whose failure domain is typed (`ExecutableResolution`, `EditTarget`,
`ResourcePath`, `ProjectMarker`) opens an `ExecutionObligation` with a
stable scope identity and a per-epoch precondition fingerprint. For
process launches the host-owned resolver defines resolution explicitly —
absolute paths as-is; separator-relative forms join the call cwd; bare
names search the cwd first, then effective PATH (PATHEXT-aware on
Windows) — and stamps both the stable scope digest and the epoch
fingerprint (full bounded cwd state + effective PATH + canonically
sorted env overrides) into success *and* failure metadata: preflight,
RetryDomain and spawn share one interpretation. Source edits do not move
the epoch; a build that changes the directory state does.
Rules: unrelated progress never resolves an obligation; world movement
advances the epoch (**PreconditionChanged ≠ ObligationResolved**) and
keeps total attempts; resolution requires blocker-specific proof — a
launch success with matching scope key *and* fingerprint, or the target
identity proofs for the other domains; `NonDeterministic` domains open
nothing. TASK PROGRESS renders at most two bounded UNRESOLVED BLOCKER
lines beside the global advisory, and every ledger transition is
event-visible (`ExecutionObligation`). Evidence argument identity uses
the Runtime-computed `ArgumentDigest` (not producer strings), so
same-argv/different-env and same-path/different-cursor calls no longer
collide on evidence identity.

### Protocol working set (turn checkpointing)

The current turn is itself a working set. The wire view of `TurnFrame`
keeps only the last `TURN_FRAME_KEEP_EXCHANGES` (6) completed tool
exchanges; older ones compact to a bounded deterministic
`TURN CHECKPOINT` note injected right after the user directive. The note
contains the compacted count plus at most six distinct recent receipts from
the compacted prefix. A receipt is only `tool name + ok/failed + bounded short
summary` (96 characters maximum). Only `PersistObservation` results qualify;
tool arguments, raw/model content, artifacts, and transient context retrieval
never enter it. Producer output is normalized to one line and rendering
re-applies both row/count bounds to old or deserialized inputs.

Whole call+result groups are dropped so the wire protocol keeps every tool
call paired with its result; the trailing (possibly in-flight) region is
always retained. The runtime's full frame is never mutated — audit,
`ToolFinished` events, and turn-end persistence still see every step.
`ModelInput.turn_checkpoint` records the count and ephemeral receipt index;
`ModelStarted.turn_checkpoint` durably records only compacted/receipt counts,
and token accounting (`PromptLayerCosts.turn_frame_tokens`) measures the wire
view. Old traces/checkpoints default the new fields to empty/zero.

No LLM summary is involved. Receipts prevent checkpointing from erasing which
checks or failures already occurred, but explicitly do **not** assert
currentness: current operational facts still come only from typed
TaskProgress / Execution Frontier evidence, and exact bodies still come from
the retained tail, a selected context body, artifact/reference retrieval, or
the current-turn protocol body cache. The journal remains audit backing, not
a model-visible fact source. This is an execution-protocol optimization; it
does not modify ContextEngine materialization, selection, GC, residency,
reactivation, or token budgets.

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

## Long-task continuation boundary (planned Runtime slice)

The current state model is already sufficient for bounded continuation:
`TaskAnchor` owns goal/constraints/acceptance/plan/open loops, and the existing
`TaskRecord.resume: ExecutionState` owns checked revisions, verification,
failed commands and obligations. Context owns selected evidence; `TurnFrame`
owns only the active execution stack. Do not add a second task table, another
ResumePoint, or a transcript snapshot.

What is not yet complete is the durability boundary inside one long user
directive. Today the final turn projection is installed on `TaskRecord.resume`
after the durable `TurnCompleted` barrier, and full Runtime checkpoints are
requested externally. That is correct for ordinary short turns, but a process
restart during a long tool loop has no committed, bounded continuation point
for the already settled substeps.

The next Runtime slice is therefore state-driven safe-point continuation:

1. A safe point exists only after the entire requested tool batch has terminal
   settlement, every prepared effect has committed or rolled back, required
   authority/event records have landed, and no operation is in flight.
2. Meaningful durable changes accrue bounded checkpoint-debt reasons (task
   progress, durable workspace mutation, verification, suspend/pause,
   completion, shutdown). Reasons coalesce; a fixed round count is not a
   semantic trigger.
3. At that boundary Runtime may install the bounded `ExecutionState` into the
   existing task resume and write one atomic full Runtime checkpoint. Raw tool
   bodies remain artifacts and raw transcript history is not serialized.
4. Restore uses the existing authority marker and cross-plane validation. A
   Runtime-owned `continue_active_task` command may then create a fresh
   `ActiveTurn` from the same task id, current directive, TaskAnchor and
   ExecutionState. It does not mint a user instruction or advance the
   directive revision.
5. Explicit pause/suspend/completion/shutdown waits for a durable checkpoint
   acknowledgement. Background failure keeps checkpoint debt visible and
   retryable; it cannot claim that the task is safely resumable.

The bounded next action/open-loop update reuses the proposed Runtime-owned
`task.manage` control with TaskAnchor CAS. Its first slice is task-required or
catalog-cold, not globally always visible. It may update autonomous progress
fields but cannot rewrite user constraints. This is an explicit model
proposal, not automatic free-text extraction and not a Runtime planner.

This planned slice changes the current invariant that only `TurnCompleted`
installs durable resume state, so it must land with a distinct durable
safe-point event/barrier and cancellation/restart tests before that invariant
is revised. Until then, do not claim mid-turn crash continuation.

Detailed evaluation and the first one-directive development fixture are in
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

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
