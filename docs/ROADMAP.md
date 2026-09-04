# Prototype Roadmap

Current milestone authority. Dates live in git history, not in this
header. A milestone is complete only when its named acceptance holds,
not merely when one implementation path exists.

Design: [`ARCHITECTURE.md`](ARCHITECTURE.md),
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md),
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md),
[`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md).
Long-task development diagnostic:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).
Now/freeze: [`STATUS.md`](STATUS.md).
Defects: [`AUDIT_TODO.md`](AUDIT_TODO.md).

Historical P0–M11 landing notes are in git history of this file. Do not
copy them back.

## Product destination

The repository is aiming at a **reliable single-user local coding Agent**, not
at a general planner or a distributed Agent platform. V1 is complete when a
user can install one versioned build, point it at one workspace, give it one
repository-level task, approve bounded effects, inspect the result, and resume
after interruption without replaying an uncommitted suffix or duplicating an
effect.

That outcome requires six user-visible properties:

1. startup validates the workspace, provider profile and helper processes
   before entering the interactive UI; mock mode is explicit;
2. the Agent can inspect, edit and run host-owned verification through the
   existing bounded tool and approval paths;
3. task status exposes the current goal, next action, blockers, latest trusted
   verification and recovery debt without making the UI a new authority;
4. checkpoints use the existing verified checkpoint store and a fresh process
   can explicitly resume the latest compatible safe point;
5. completion produces a bounded review summary with changed files,
   verification and unresolved limitations; and
6. the packaged Windows and Linux binaries pass startup, one representative
   coding flow, cancellation and cold-resume smoke tests.

Vectors, a learned planner, TaskGraph, background workers, multi-Agent
delegation and Self-Iteration are not V1 requirements. They remain
evidence-triggered candidates after the simpler product has passed its gates.

## Current gates

| Milestone | Status | Gate |
| --- | --- | --- |
| M10 Runtime Consistency | ✅ repair landed, re-audit recorded 2026-09-03 | `RUNTIME-CONTEXT-COMMIT-01` repairs landed (`9ba85d3`/`f42a898`/`f622cf3`) with rollback fencing and scratch-state restore validation. The M10 fault gate was re-run and recorded on `e357bed` (2026-09-03): all runtime/replay/storage/context fault and restore-consistency suites green; record in `RUNTIME-CONTEXT-COMMIT-01`, [`AUDIT_TODO.md`](AUDIT_TODO.md). |
| M11 Context Recall | ✅ narrow retrieval | Search/inspect/fetch without polluting prompt history. Broader catalog work is not a reason to reopen recall. |
| M12 Effect Runtime | 🧾 closure-audit evidence banked; recovery P0 repaired, claims suspended | The clean-tree evidence table remains immutable. `EFFECT-ACK-CLASS-01` (typed settlements, journal v2, no-strengthening recovery) and `PROCESS-COORDINATOR-01` (bounded coordinator wire) are repaired in `6112ffd`/`43eb87b`. No new closure claim until the recorded 2026-09-03 doc tranche and the M15-facing exits land. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). |
| M13 Extension Sandbox | 🧾 closure-audit evidence banked; attestation P0 repaired, claims suspended | The clean-tree audit remains immutable. `SANDBOX-ATTEST-TRUNCATE-01` is repaired in `e5e712f` (write-floor attestation only at enforcing ABIs). Universal native `UntrustedGenerated` availability is not V1 and WASI remains V2. No new closure claim until the recorded 2026-09-03 doc tranche and the M15-facing exits land. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). |
| M14 Resource Policy | ✅ | Schema/context quotas, standing grants, output broker, authority leases. Further typed policy is not a reopen of this gate. |
| M15 Real Evaluation | 🛑 open; nine v4 valid FAILs + one censored banked, latest 11/12 (best yet) | Windows remain immutable diagnostics. The ninth window (2026-09-04, source `6bf651b`, PinAI `gpt-5.6-luna` tuple, after the serving outage lifted) is a valid FAIL 11/12, 0 NOT_RUN — diag 3/4 (one stochastic overflow-edge miss), migrate 4/4, and policy 4/4 for the first time including two completed closures. Per M15_ACCEPTANCE §5 the bf52490 candidate is rejected and the window is not rerun. The two stochastic model-behavior surfaces have each swept 4/4 in some window but never simultaneously; all infrastructure causes are repaired and exonerated. A new candidate needs diagnosis, deterministic gates, a fresh preflight and a fresh predeclared window. Formal M15 remains 3 fixtures × normal/resume × 2 repeats, never settlement off/on. See [`M15_ACCEPTANCE.md`](M15_ACCEPTANCE.md). |
| V2 Self-Iteration | 🔒 blocked | Until the governing M12/M13 status is reconciled and M15 closes. The agent may grow capabilities, never evaluation or permission Core authority. |

Open gate order (post-repair, updated 2026-09-03): ~~M10 fault-gate re-audit
record~~ (recorded) → ~~wire the typed retry observer into the formal
long-live/M15 provider path~~ (done on `7e02488`) → ~~`GOV-STATUS-01`
reconciliation~~ (closed on `bba1c76`) → ~~commit pending tests/docs~~ (done
on `e357bed`/`bba1c76`) → ~~record one clean
source with Ubuntu/Windows CI~~ (recorded: `4e56f69` code, run
`33663057012`) → ~~selected-path P1 disposition for this candidate~~
(accepted for the frozen run; repository-wide residuals remain in
`AUDIT_TODO.md`)
→ ~~same-checkpoint causal runner~~ (settlement-off candidate skips it) → ~~new exact-source product preflight~~ (relay NOT_RUN retained; PinAI tuple
PASS 2026-09-03) → ~~predeclared M15 window~~ (ran 2026-09-03: valid FAIL
6/12, `_windows/1788385151733`) → ~~attempt-incident candidate selected
2026-09-03 (`e897c5c`), deterministic matrix + clean source + dual-platform
CI green, product preflight PASS on `51559d4`, at most one freshly
predeclared window~~ (ran 2026-09-03 on `38d458e`: valid FAIL 10/12,
`_windows/1788402676712`) → ~~completion-gate convergence candidate selected
2026-09-03, deterministic matrix + clean source + dual-platform CI green,
exact-source preflight PASS on `2adad31`, at most one freshly predeclared
   window~~ (ran 2026-09-03 on `a6dc33e`: valid FAIL 10/12,
   `_windows/1788438275930`) → ~~successor reliability candidate selected,
   exact-source preflight PASS on `1651354`, censored 10/12 window retained,
   authorized unchanged rerun~~ (valid FAIL 6/12,
   `_windows/1788466134988`) → semantic completion-liveness candidate selected
   and implemented in the 2026-09-04 working tree; its focused deterministic
   matrix is green and the full gate/CI route remains open. The platform audit
evidence stays banked, M14 is not reopened, and Context/GC/retrieval/packing
remain frozen.

## Immediate M15 route

1. Preserve every valid FAIL, the censored successor window at
   `_windows/1788463526600`, and its authorized valid-FAIL rerun at
   `_windows/1788466134988`. Do not rerun a rejected source/serving candidate.
2. Use the immutable cells and
   `evidence/m15-diagnosis-successor-rerun/REPORT.md` as the diagnosis basis.
   Keep the stochastic diag overflow edge separate from the recurrent policy
   completion tail unless typed evidence proves a shared cause.
3. The materially new candidate is selected: semantic completion liveness
   (`COMPLETION-LIVENESS-01`). It must not retune Context/GC/retrieval/packing,
   weaken completion/effect/recovery gates, add prompt pressure or introduce a
   second planner/orchestrator.
4. Pass its deterministic regressions, applicable open P1 exits (or exact
   selected-path exclusion), then record one clean source with the complete
   local gate and Ubuntu/Windows CI. Basic tests are not this exit.
5. This candidate does not enable settlement projection, so it skips the
   same-checkpoint `EVAL-CAUSAL-01` fork.
6. Once a supported serving is available, freeze source, surface, acceptance
   identity and serving; pass one bounded exact-source product preflight, then
   predeclare and run at most one 12-cell v4 window. A valid FAIL rejects the
   candidate; only typed `NOT_RUN` permits rerunning the whole frozen window.

### Workload disposition after the 6/12 FAIL

The 2026-09-03 "one-stop reliability" proposal is assessed as a large umbrella,
not a candidate selection. Its operational gate runner, Runtime convergence
semantics, Provider buffering and any public property verifier have independent
authorities and acceptance risks; they must not be bundled into one change and
then credited with one aggregate result.

- The attempt-incident admission candidate was selected from the 6/12
  diagnosis (`e897c5c`, 2026-09-03) and has now run its own predeclared
  window: valid FAIL 10/12 on `38d458e` (`_windows/1788402676712`). Step 3
  above is therefore open again: select one materially new product/serving
  candidate before changing formal evidence.
- The completion-gate convergence candidate was selected from that attempt-incident
  diagnosis as its explicit recommendation, closed the diag tail (diag 4/4),
  and has now run its own predeclared window: valid FAIL 10/12 on `a6dc33e`
  (`_windows/1788438275930`). Its two residual `retry_policy_dev` failures are
  an uncovered execution-debt tail and a Runtime restore/storage lifecycle
  failure at resume restore
  ([`evidence/m15-diagnosis-completion-gate/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-completion-gate/REPORT.md)).
  Step 3 above is therefore open again: select one materially new product/serving
  candidate before changing formal evidence.
- A user-authorized successor repair set now has deterministic coverage for
  the two latest infrastructure/runtime causes plus the recurring local test
  failures: the surviving debt is correctly attributed to an unrooted
  speculative missing read (the same-digest formatting PASS had already
  cleared its row); cancellation-lag model futures no longer retain workspace
  locks; eval CLI/Python/sidecar startup is parse-first and semantically
  probed; and buffered capacity is typed without changing its limits. This is
  successor-source implementation, not a retroactive window repair or a new
  formal verdict. It must still satisfy step 4 and, if selected for M15, the
  fresh step-6 sequence.
- `EVAL-PREFLIGHT-01` may improve the developer/evaluation gate independently,
  but it neither repairs a valid cell FAIL nor authorizes a formal window.
- A Runtime candidate must reuse the sole `CompletionReadiness`. Repair is a
  durable semantic episode with typed postconditions and an ordered blocker
  potential; prose or volatile revision churn is never progress. It may not add
  another completion coordinator, infer debt resolution from prose/raw round
  count, auto-clear blockers or auto-complete.
- A buffered-stream change must preserve independent byte, chunk and wire
  bounds and fail closed. Raising or deriving away the 16,384-chunk bound is not
  accepted without deterministic evidence that normalization preserves the
  resource floor.
- A public counterexample/property verifier changes the task and acceptance
  surface. It is not a repair to the frozen diag fixture and requires an
  explicit prospective refreeze before it can enter a future formal candidate.

Detailed slice sizes and gates are in
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md#2026-09-03-workload-split).
The frozen M15 acceptance text is unchanged.

## Route to a usable local Agent

This is the only product delivery order. Defect details stay in
[`AUDIT_TODO.md`](AUDIT_TODO.md); formal evaluation semantics stay in
[`M15_ACCEPTANCE.md`](M15_ACCEPTANCE.md). Sizes are relative implementation and
verification effort, not calendar promises.

| Phase | Outcome | Exit | Size |
| --- | --- | --- | --- |
| 0. Record a trustworthy successor | Finish the current reliability tranche without changing frozen evidence. The `b44ea44` local doctor passed. Source `c84f85e` then passed Windows/Linux format, Clippy and build, but Ubuntu tests exposed a separate fd-number reuse race in a Landlock drop assertion. | Make the assertion verify the original fd identity rather than numeric availability, rerun the complete local gate, obtain green Windows/Linux CI on one clean source, and record which open P1 items apply to the next M15 path. No preflight or formal window is chained automatically. | S–M |
| 1. Close M15 | Select one materially new candidate from the immutable diagnosis and run the frozen sequence exactly once. | Deterministic regressions, applicable P1 exits or exact exclusions, clean dual-platform source, one product preflight, one predeclared 12-cell window, 12/12 mechanical PASS. | M, plus external serving time |
| 2. Reliable Local Agent alpha | Turn the existing runtime into a coherent product path: checked CLI/provider configuration, explicit demo mode, visible bounded command errors, understandable approvals/grants, verified checkpoint save/list/resume, and user-visible task/verification/recovery status. Default scope is dynamic in-process Context, builtin tools, one workspace and an interactive TUI. | One clean-machine flow completes read → edit → verify → review → complete; kill/restart resumes a compatible safe point without duplicate effects; invalid config/checkpoint/debt fails before mutation. Close `COMPOSE-LIFECYCLE-01`, `CONTEXT-IO-01` and `EFFECT-ACK-01` for this path — all three closed 2026-09-04 (`c7ed011` and the tranche CI-verified on `1c4b0e0`, run `33809422347`). Keep service Context experimental unless `SIDECAR-ERROR-01` closes. | L |
| 3. Local Agent V1 release | Package the alpha for Windows/Linux and test representative task breadth rather than adding orchestration concepts. Reuse `LT-EVAL-06` normal/cold-resume twins and record the actual product round/tool limits. | Versioned binaries and checksums, install/upgrade notes, packaged-binary and TUI smoke tests, bounded diagnostic export, and all three `LT-EVAL-06` task families passing their predeclared behavior/diff/verification/closure/runtime/restore gates. | M–L |
| Later, only from evidence | Add the smallest rebuildable run/task projection needed by real status queries. A persistent Chronicle, serial TaskGraph, worker or extension SDK is considered only after repeated task-family evidence shows the simpler `TaskAnchor + ExecutionState` substrate is insufficient. | Separate proposal and acceptance for each capability; no second `TaskManager`, orchestrator, authority log or trace database. | Conditional |

Phase 2 should extend the existing product host and checkpoint/event projections;
it should not begin with a daemon, web UI, database, plugin marketplace or a
new planner. Product configuration may expose a checked execution-round cap,
but the current TUI default and the formal M15 cap must be recorded separately;
the frozen M15 design is not rewritten to match a UI choice.

## Post-M15 candidate order

This order is conditional on M15 closing and is summarized here so architecture
research cannot outrun the product:

```text
M15
  -> Reliable Local Agent alpha
  -> LT-EVAL-06 + packaged V1 release gate
  -> smallest rebuildable Run/Task status projection, if needed
  -> conditional serial TaskGraph, only if breadth evidence requires it
  -> worker / multi-Agent execution, only after an accepted protocol gate
  -> Self-Iteration, still separately blocked
```

The first product status view should be a bounded read model over current
Runtime events, task state, validated checkpoints and recovery debt. A broader
Chronicle is optional. If selected, it is disposable and rebuildable, cannot
drive effect commit or recovery, and cannot invent `StepRecord` identities
before a separately accepted typed-step contract. Raw traces remain filesystem
artifacts; no database is accepted merely to make querying convenient.

TaskGraph remains conditional on recurring breadth evidence and starts
serially if accepted; a Chronicle is not automatically required. Worker
parallelism, multi-Agent delegation and recursive improvement stay later
because they add workspace conflicts, leases, consistent-cut checkpointing and
promotion authority. M12/M13 claims remain suspended as recorded above, and
Self-Iteration remains blocked by the governing gates.

## Historical implementation notes (non-ordering)

The following dated items retain evidence and rationale only. Their original
numbers do not extend or reorder the six-step immediate M15 route above.

7. Execution Convergence V1 candidate gate (revised 2026-08-23 second
   review; lineage metrics added after the obligation-run evidence):
   before any V1 candidate claim, all of the following hold —
   (a) the Convergence Bench is green: four deterministic
   scripted-model scenarios (`retry_domain`, `operational_evidence`,
   `protocol_body`, `verification_reuse`); (b) the event-visible Obligation Ledger shows no
   unresolved blocker with excessive same-epoch attempts and no lineage
   accumulating repeated failed epochs without resolution
   (`max_obligation_attempts_per_epoch`, `max_total_attempts_per_lineage`,
   from `ExecutionObligation` events); and (c) hidden verification is
   green on the live A/C longflow cells. The global
   `frontier_no_advance_peak` metric stays a diagnostic only: C r2
   proved it can stay under threshold while a 13-attempt process-guessing
   loop runs, because interleaved unrelated advances reset the counter.
   The later retained-run reaggregation makes the split explicit: C/A had
   equal outcome advances (8/8), but evidence-only results 48/21, Unknown
   invalidations 9/0, and maximum outcome-free result streak 18/3.
   `outcome_frontier_*` remains an eval shadow, not a hard stop or planner.
   Evidence identity uses the Runtime `ArgumentDigest`; cache hit-rate
   claims must be backed by `ProtocolBodyCacheStats` events. This gate
   does not close M12/M13/M15 and does not reorder them.
8. The first three execution-flow behavior slices landed 2026-08-24: exact
   result-delivery sources renew on reuse and release unrooted schemas to Warm
   after consumption/new-directive boundaries; explicit task and typed need
   roots survive. Model-explicit loads now remain pending until exact use,
   unload, or directive end, so adjacent loads can form a small source-driven
   cohort without a round TTL. Host/operator loads are a separate
   explicit-unload source; Runtime/model loads never become task-global pins
   and restored residency cannot mint host intent. Body-free
   `ExecutionBatchSettled` accounting includes
   transient, refused, reused and atomically refused oversized batches. Event
   append failure fences before the next model decision. The second slice adds
   fail-closed pre-dispatch execution attribution, live-checked
   revision-bound negative path facts, lifecycle metrics, and anchor-bound
   exact verifier source affinity. Speculative misses do not become task
   blockers; rooted misses still do, and shell/process or producer metadata
   cannot mint verification. The third slice adds host-opt-in
   `ExactCurrentWorld` PASS reuse keyed by task/anchor/directive/workspace,
   exact tool/arguments and a trusted recipe/profile/policy/environment
   identity digest. Reuse is a truthful audited terminal result; any identity
   change dispatches. A bounded `verify.run { recipe_id }` production entry
   point now shares one host recipe set with Core authority; general project
   runners are TaskScoped, while the generic standalone-Rust compile recipe is
   the first exact source-read-only builtin and binds a complete bounded
   workspace input snapshot, platform, compiler and environment. Unsafe or
   incomplete capture and pre/post identity drift downgrade instead of reusing.
   The deterministic real-runtime bench proves two terminal requests from one
   spawn. Continue with broader coverage and
   obligation provenance, identical in-flight joins, host-declared equivalence
   classes, stale settlement and obligation-scoped source roots from
   `EXECUTION_COHERENCE.md`. Required/core tools, active/result-delivery calls,
   explicit Task requirements and trusted unresolved obligations must remain
   available. Do
   not select a call-count/round TTL from one A/C trace or lower the global
   schema watermark as a substitute. After deterministic coverage,
   require at least two paired live repeats; hidden success and outcome count
   cannot fall, median rounds/calls must fall, and p95/max turn cannot gain a
   new tail. Context selection/GC/retrieval stays frozen.
9. Before adding another model-visible execution-progress hint or generic
   "stop earlier" standing instruction, first freeze a deterministic
   already-satisfied-task replay plus at least two paired live repeats. A live
   repeat is eligible only after its evaluator proves exact resume-artifact
   correlation, typed NOT_RUN/setup failures, full mandatory PASS conjunction
   and complete failed-path accounting.
   Acceptance requires lower median rounds and tool calls, unchanged hidden
   success, and no new long-tail constraint turn. The rejected
   `TaskProgress.task_changes` and current-workspace-authority experiments show
   that a locally useful hint can create global prompt/control coupling. Keep
   Context selection, GC, and retrieval frozen while evaluating execution-only
   candidates.
10. Tool-contract changes must distinguish invocation prevention from failure
   masking. Do not expose optional opaque capability fields on every
   first-page file/search tool: shape validation made the model fabricate
   plausible-looking identities, while empty normalization merely hid the
   same call. Use the existing bounded `artifact.read` as the one model-visible
   continuation primitive; keep legacy per-tool cursors parser-only and
   fail-closed. Union-shaped meta-tools may ignore malformed placeholders only
   for fields the selected operation does not consume; required and relevant
   fields stay strict. Deterministic tests establish contract safety, but
   acceptance still requires fewer paired-live rounds and calls at unchanged
   hidden success and without a new tail. The two 2026-08-24 cursor runs are
   negative controls; the strict-schema follow-up also validates the
   operation-aware `context.manage` parser (4/4 successful calls).
   Apply the same ownership rule to completion: Runtime-owned assistant and
   verification refs are attached by Runtime, not echoed as optional opaque
   fields by the model. Keep parser compatibility, and grade the change on the
   paired round/call gate rather than proposal-error count alone.
   Treat ordinary turn completion and durable task closure as separate
   lifecycle decisions. In the current v5 production registry `task.complete`
   is always loaded; the JSON inventory and conformance view must match that
   actual surface (`TOOL-MANIFEST-01`). Visibility is not authority: a clean
   proposal may terminalize only when the shared `CompletionReadiness` gate
   accepts it. A clean accepted proposal may terminalize at the
   settled-batch safe point, but any failed sibling or invalid verification
   gate must return to the model. Two initial 2026-08-24 intent-gated pairs reduced C
   from 74–77 rounds / 84–92 calls to 49/44 and 57/52 while retaining the
   Context advantage. The candidate remains behind the success-neutral gate:
   C hidden success was 3/4 then 4/4 versus A 4/4 twice. A later complete pair
   passed 4/4 in both arms but regressed C to 82 rounds / 76 calls with one
   30-round edit-repair turn versus A's 47 / 36. The task-close loop stayed
   absent; the trace instead exposed ordinal repair anchors and a prefix-only
   edit echo hiding the final changed tail. Keep `edit.patch` exact and
   revision-checked, but expose only unique anchors to the model and preserve
   both ends of bounded success echoes. Legacy ordinal input is parser-only.
   Require a new unchanged-workload repeat with hidden parity and no max-turn
   regression before declaring either candidate accepted.
   The first such post-hardening pair passed 4/4 in both arms and restored C to
   53 rounds / 51 calls / max-turn 7 versus A's 54 / 44 / 13; C retained 37%
   lower input and had no failed tool outputs. Three remaining C calls read
   zero-byte verification artifacts, so process tools now suppress
   `artifact_ref` only when the sealed capture has zero bytes. Preserve all
   non-empty artifacts and do not subtract those calls from recorded evidence.
   Require an independent repeat after this output correction before
   acceptance.
   The post-output repeat passed hidden 4/4 in both arms and removed all empty
   artifact reads, but C still used 47 rounds / 44 calls versus A's 43 / 32.
   Its twelve-call gap was evidence/discovery rather than edit repair: 29
   evidence-only results versus A's 16, with one targeted already-satisfied
   turn broadening into Git and capability discovery. Refine the advisory
   Evidence Frontier, not Context selection: after an exact Fresh directive
   target exists, retain novel unrooted evidence without counting it as task
   progress; preserve broad exploration when no exact target exists. Co-locate
   exact current-body identity and keep verification recipe ids out of the
   tool-name namespace. This correction requires a new unchanged-workload
   pair before acceptance; do not infer its effect from r5.
   That r6 pair rejected the advisory as a convergence fix: C preserved hidden
   4/4 and its Context advantage but used 57 rounds / 56 calls / max-turn 15
   versus A's 49 / 38 / 7. One already-satisfied turn repeatedly alternated
   catalog control and Git reads. Surface events prove that loading
   `git.status` displaced the still-unused `git.diff` at the next decision.
   Keep the truthful task-frontier classification, but fix the lower-level
   source lifetime: pending explicit loads coexist until exact use, unload, or
   turn end, while called-tool result delivery remains one decision. This
   cohort correction has deterministic coverage but postdates r6; require an
   unchanged-workload pair and no new max-turn tail before acceptance.
   The r7 pair verified the cohort lifecycle (`git.status` 7→1 and `git.diff`
   2→1; max-turn 15→8) but C still used 62 rounds / 59 calls versus A's
   46 / 35. The remaining control plane was dominated by compact core coding
   primitives. Evaluate one isolated stable-core candidate: always surface
   `fs.write`, `git.status` and `git.diff` (combined ~190 schema tokens;
   production core ~947/4,096). Shell/process, `edit.replace`, Context/task
   control and plugins remain dynamic. Revert if the next unchanged pair does
   not lower rounds/calls at hidden parity and retained Context advantage.
   Three pairs now support retaining the stable core: r8 C/A was 46/46 rounds,
   41/37 calls; r9 was 49/47, 46/38; every arm passed hidden 4/4 and C retained
   a large Context/input advantage. Formal convergence stays open because r9
   C max-turn was 9 versus A's 7 and the remaining tail was a conflicting
   multi-hunk edit. The next isolated editor candidate requires explicit
   `replace` / `insert_before` / `insert_after` intent while keeping unique
   exact anchors, revisions, EOL fidelity and settlement unchanged. Omitted op
   is parser-only replace compatibility. Deterministic coverage and the
   unchanged r10 pair are green: C/A was 48/47 rounds and 41/39 calls, with
   identical hidden 4/4, three failed outputs and max-turn 8. Across r8-r10 the
   median gap is +1 round / +4 calls and C retains the large Context advantage.
   Keep this boundary. Do not reopen Context or the surface set, and do not add
   positional/fuzzy edit authority from one ambiguous-anchor sample. Consider
   bounded revision-bound exact guard context only if further unchanged live
   evidence makes ambiguous-anchor rereads a material residual tail.
11. Repair and re-prove bounded long-task continuation before treating it as
    an autonomous-agent evaluation. `LT-RUN-01..03` and useful `LT-RUN-04`
    primitives remain; the 2026-08-27 audit reopens snapshot, verification and
    evaluator correctness. Reuse `TaskAnchor` plus
    `TaskRecord.resume: ExecutionState`; do not add a second ResumePoint or
    persist raw transcript history.

    **Runtime correctness gate**

    - actor-owned, lineage-persisted monotonic snapshot sequence independent of
      task-anchor CAS;
    - two-phase completion whose prospective terminal checkpoint is internally
      valid, bounded, durable and fresh-restorable before in-memory commit;
    - no-debt/no-failed/no-in-flight continuation fence;
    - exact lineage/task/sequence/artifact/checksum/capability-generation
      acknowledgement;
    - stable capability-generation capture on automatic and explicit paths;
    - one shared verification-basis predicate across completion, resume, reuse
      and `CompletionOpportunity`.

    **Evaluator validity gate**

    - harness-owned oracle setup is explicit and setup/start failures are
      typed NOT_RUN rather than behavior failure;
    - provider, Runtime, behavior, diff, closure, restore and continuation are
      independent truth dimensions;
    - PASS requires every mandatory dimension and no runtime error;
    - failed/cancelled/timed-out cells retain their real round/call/token totals;
    - cancellation follows only the latest exactly acknowledged snapshot; and
    - exact verification binds the complete bounded workspace input set.

    **Deterministic exit**

    Exercise same-anchor multi-snapshot ordering, out-of-order acknowledgement,
    new debt during an old write, failed-write retry, final-artifact restore,
    stale capability generation, progress-only verification movement and
    report reconstruction. Raw prior evidence is retained but diagnostic.

    **Live exit (completed 2026-08-28)**

    The frozen retained-C `CompletionOpportunity` off/on normal/resume gate ran
    after deterministic exit and failed promotion. That mechanism has ended
default-off and must not be rerun or repackaged as Completion Convergence
V1. No Context/GC retune, fixed round-cap trick, provider-specific policy or
standing “stop earlier” instruction is allowed. Detailed contract:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

The current completion-liveness candidate does not contradict that rule. Its
terminalization is driven by a durable semantic episode that observes no
strictly better typed blocker potential; it stops further actions in the turn
without claiming completion. It is not a global model-round cap.
12. V2 Self-Iteration last.
