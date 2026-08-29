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

## Current gates

| Milestone | Status | Gate |
| --- | --- | --- |
| M10 Runtime Consistency | ✅ | Runtime and context never split-brain on task/restore/turn commit. |
| M11 Context Recall | ✅ narrow retrieval | Search/inspect/fetch without polluting prompt history. Broader catalog work is not a reason to reopen recall. |
| M12 Effect Runtime | ✅ closed 2026-08-27 | The bounded evidence table proves every brokerable production effect crosses the common reserve/dispatch/ack path, crash windows reconcile as NotApplied/Applied/Ambiguous, authority/revocation fencing holds, and generic shell/process are explicit non-transactional exceptions (`crates/agent-eval/evidence/platform-closure/m12/`, clean-tree PASS; regenerate via `agent-eval --platform-closure-m12`). Requester-side application is the V1 contract; broker-owned remote execution is not required without a remotable consumer. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). |
| M13 Extension Sandbox | ✅ closed 2026-08-27 | Clean-tree closure audit: real-child activation per supported profile with post-spawn mechanism attestation, both required refusals (missing write confinement; native untrusted floor), and contract negatives — zero unresolved rows (`crates/agent-eval/evidence/platform-closure/m13/`, regenerate via `agent-eval --platform-closure-m13`). V1 acceptance is truthful fail-closed activation, not universal native availability. Running untrusted generated code through WASI remains V2. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). |
| M14 Resource Policy | ✅ | Schema/context quotas, standing grants, output broker, authority leases. Further typed policy is not a reopen of this gate. |
| M15 Real Evaluation | 🧪 three valid v3 windows failed; task-aware convergence landed steps 1–4 but its paired gate FAILED promotion; projection stays off | The typed v3 harness and calibrated fixtures are valid. Three clean pinned-serving windows ran 2026-08-29 and failed 11/12, 9/12 and 10/12; migrate + policy are 24/24, formal diag is 6/12, and the sole recurring failure is the calibrated saturation edge. Task-aware Completion Convergence landed all four delivery steps 2026-08-29 and ran the true projection off/on gate on an approved 8-cell budget: 8/8 cells PASS, 0 NOT_RUN, but the verdict is FAIL on strict parity (pair-0 exposure asymmetry, marker needle-shape parity in 3/4 pairs, episode medians 1→1). The projection stays default-off and the gate returns to observation. **Not closed.** Only a promoted projection may enter the next exact-source preflight, and the prospective cross-window rule is frozen in [`M15_ACCEPTANCE.md`](M15_ACCEPTANCE.md): no fourth unchanged window or sampling until a lucky 12/12. |
| V2 Self-Iteration | 🔒 blocked | Until M12/M13/M15 close. The agent may grow capabilities, never evaluation or permission Core authority. |

Open gate order: M12 ✅ → M13 ✅ → V1 candidate → evaluator validity ✅ →
Task-aware Completion Convergence → real projection off/on gate (ran
2026-08-29; FAILED promotion, back at observation) → freeze the
cross-window decision rule → exact-source preflight → one predeclared formal
M15 window → V2 Self-Iteration. Both platform gates closed on their named
clean-tree
evidence (2026-08-27); M14 is already closed and is not reopened or inserted
back into the active sequence. Context live evidence runs in parallel and does
not retune GC. Tool Surface edit reliability may improve in parallel, but it
does not reorder or close any gate.

## Ordered route

1. ~~Close M12 by the bounded production-path/crash/reconcile/fencing evidence
   table~~ — done 2026-08-27 without making raw shell transactional or waiting
   for remote execution.
2. ~~Close M13 by honest structured attestation and fail-closed activation~~ —
   done 2026-08-27; universal native `UntrustedGenerated` availability still
   waits for WASI/V2 by design.
3. Keep M14 closed; do not reopen it as a sandbox dump.
4. V1 candidate: `context-mech.v2` 12-cell Context evidence exists; do not
   retune GC from it. Separately, `edit.patch` v4 removed confirmation reads
   and, after the gate accepted byte-equivalent decompositions as `v4`, one
   archival 4x3 window reached `strict 12/12 gate 12/12` with zero confirmation
   reads. The product contract is byte/revision/settlement truth; hunk
   partition is not model-visible authority and no current consumer requires a
   golden decomposition. Do not wait for another provider or repeat the
   unchanged ambiguous gate. Deterministic crash, external-race,
   journal-corruption and portable
   disk-full coverage remains required. This finite diagnostic is not a general
   failure-rate estimate and does not close M12, M13 or M15.
   `TOOL-DIR-01` is also deterministically complete: `fs.mkdir` is a
   single-component, pinned-parent, authority-v3 directory effect and
   `fs.write` still requires an existing parent. The `TOOL-DIR-SURFACE-01`
   deterministic admission gate landed (2026-08-28): a typed missing-parent
   refusal surfaces exactly `fs.mkdir` with `RecoverySurface` provenance for
   one decision, approval unchanged, unrelated missing reads unaffected.
   Its full 24-cell live run had zero `RecoverySurface`/`next_directory`
   exposure; all eight policy cells instead catalog-loaded and called
   `fs.mkdir`. The off/on tail is therefore not attributable to the switch.
   Keep the baseline and switch off as `NOT_EXERCISED`. Mechanical exposure
   accounting, workspace-cleanliness alignment and diagnosis calibration
   landed as observation/evaluator work (2026-08-29):
   overflow-safe golden, directive names the saturation edge, hidden check
   demands an overflow-safe marker, and self-check runs both pack oracles
   offline. Do not advance the always-ready fallback from this run; the open
   work is the separate task-aware convergence join below.
5. Task-aware Completion Convergence is the bounded pre-M15 readiness task.
   Keep the landed labels/events as observation, but reserve
   `SettledCandidate` for current task authority plus current verification,
   zero in-flight work/obligations/known failures/open loops/next action, and
   explicit bounded acceptance coverage. Without task-level coverage, stop at
   `VerifiedCurrent`. New directives, boundary changes, mutations, failures or
   stale proof reopen work. Only after deterministic safety proof may
   `PromptAssembler` render one neutral bounded fact behind a default-off
   switch. Replace first-candidate lifetime tails with episodes that close on
   reopening, then compare projection off/on with normal/resume and at least two
   repeats in both arms. Preserve ordinary final, whole-task `task.complete`
   and concrete continuation; no auto-close, fixed-round stop, revived
   `CompletionOpportunity`, Context/GC change, transcript expansion, TaskGraph
   or learned planner.
   Landed 2026-08-29, gated the same day: all four steps shipped (task-aware
   settle with fail-closed acceptance coverage, default-off projection with
   request-level tests, settlement episodes, per-pair `evaluate_conv_gate`),
   then the off/on gate ran 8/8 cells PASS / 0 NOT_RUN and FAILED promotion on
   strict parity (pair-0 exposure asymmetry; marker needle-shape counts differ
   in 3/4 pairs; episode medians 1→1). The projection stays default-off and the
   gate is back at observation; no rerun before a bounded diagnosis of the
   recipe-choice exposure and marker-parity causes.
6. Formal M15 only from versioned per-cell artifacts. Three valid failed v3
   windows remain evidence and cannot be retried unchanged. Before a new source
   is evaluated, freeze the missing prospective cross-window decision rule in
   [`M15_ACCEPTANCE.md`](M15_ACCEPTANCE.md). After the task-aware change passes
   deterministic and paired gates, run an exact-source preflight followed by
   exactly one predeclared 12-cell window. A valid FAIL rejects that candidate
   and returns to diagnosis rather than authorizing another sample. Do not use
   one A/B/C for every layer.
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
   lifecycle decisions. `task.complete` must not be an unconditionally
   visible encouragement to close task-scoped progress after each successful
   substep: keep it catalog-discoverable and lease it from explicit closure
   intent/Task requirements. A clean accepted proposal may terminalize at the
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
12. V2 Self-Iteration last.
