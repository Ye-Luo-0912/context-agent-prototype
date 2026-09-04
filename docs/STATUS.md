# Current status

This file answers four questions only. Architecture, lifecycle, execution,
and sandbox contracts live elsewhere. Experiment facts live in
`crates/agent-eval/evidence/*/REPORT.md`. Do not treat
`docs/CONTEXT_RUNTIME_TODO.md` as live contract.

| Doc | Role |
| --- | --- |
| [`AGENTS.md`](../AGENTS.md) | Invariants, no-go, dependency rules |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Stable architecture |
| [`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md) | Context / GC / evidence / retrieval |
| [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) | ResourceFact / freshness / verification / snapshot |
| [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md) | EffectIntent / HostLifecycle / sandbox attestation |
| [`ROADMAP.md`](ROADMAP.md) | Milestone gates and ordered route |
| [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) | Long-task Runtime gaps and development diagnostic |
| [`AUDIT_TODO.md`](AUDIT_TODO.md) | Confirmed defect queue |
| [`M15_ACCEPTANCE.md`](M15_ACCEPTANCE.md) | Frozen formal-window semantics |

## Now

### Decision snapshot — 2026-09-04

**Operator update (2026-09-04):** with the M15 gate parked on serving
availability (PinAI region block / relay upstream), the operator directed
work to continue on the product route without waiting for M15. Phase 2
(Reliable Local Agent alpha) has started with its first slice: the checked
model configuration — `AGENT_DEMO=1` selects the demo mock explicitly, a
missing `OPENAI_API_KEY` is a visible startup error instead of the silent
mock fallback, and the TUI surfaces the checked selection before any
workspace mutation (`32ea622`, CI run `33814149555` green). The frozen M15
acceptance design is untouched; a serving restoration still gates any
future preflight or window.



The repository is a **substantial single-Agent runtime with a prototype product
host**. It is not yet a complete local coding-Agent product. The shortest path
is reliability and M15 first, then a deliberately small product shell; it is
not a new planner, TaskGraph, database or multi-Agent layer.

- Planning baseline: successor reliability commit `b44ea44`. Its local
  `agent-eval --doctor` run passed Python/helper probes, format, all-target /
  all-feature check, strict Clippy, build and the complete workspace test
  suite; Provider was intentionally skipped without a key.
- Follow-up source `c84f85e` fixed the Linux Clippy finding and both Windows
  and Linux format/Clippy/build jobs passed in run `33782774359`. Its Ubuntu
  part-1 tests then exposed a separate racy assertion: after `ChildRules` had
  closed an fd, another parallel test reused the same numeric descriptor, so
  `F_GETFD == -1` was not a valid proof. The current tree compares the original
  `fstat` identity when reuse wins and still requires EBADF otherwise.
- **Clean-source exit recorded 2026-09-03 on `0668002`**: run `33785349225`
  is green across both fmt/clippy/build jobs, Ubuntu test parts 1+2, and the
  Windows full suite — the new reuse-tolerant test and the reaped probe
  descendant both pass on both platforms. Directive step 1 is closed.
- By operator direction ("continue"), the **successor source is selected as
  the eighth M15 candidate**: its cancellation-lag and storage slices target
  the seventh window's diagnosed `runtime`-class restore failure directly.
  The exact-source preflight **PASSED** 2026-09-03 on the clean detached-HEAD
  worktree at `1651354` (source tree digest `21cb4095...`, serving recorded
  in the attempt manifest): `retry_policy_dev` normal, product surface,
  behavior/diff pass, closure completed, provider healthy, 40 model rounds,
  hidden oracle green.
- The 12-cell v4 window on the successor candidate + PinAI tuple is
  **predeclared 2026-09-03 before its run** (M15_ACCEPTANCE §7 item 8):
  3 fixtures × normal/resume × 2 repeats, the product surface, the pinned
  serving tuple, one uninterrupted `agent-eval --m15-window` run from the
  same clean worktree at `1651354`. Valid FAIL rejects the candidate; only
  a typed NOT_RUN permits a whole-window rerun.
- The eighth window ran from the same clean worktree at `1651354` and came
  back **CENSORED: 10/12 pass, 2 NOT_RUN** (mechanical report
  `_windows/1788463526600`): diag 4/4 — the overflow edge passed every cell
  for the first time on this serving — and migrate 4/4 all clean;
  `policy normal r1` completed in 44 rounds (the completion-gate loop did not
  recur) and `policy resume r2` passed, while `policy normal r2` and
  `policy resume r1` are typed NOT_RUN on upstream HTTP 503
  "Service temporarily unavailable". Per the frozen rule a censored window
  is not a verdict and produced no decision-grade sample, so the whole
  frozen window is rerun unchanged (same source `1651354`, same serving).
- The authorized whole-window rerun completed the same day from the same
  frozen worktree at `1651354` and is a **valid FAIL: 6/12 pass, 0 NOT_RUN**
  (mechanical report `_windows/1788466134988`): migrate 4/4, diag 1/4 — the
  overflow edge recurred stochastically in 3 cells, consistent with the
  frozen cross-window diagnosis — and policy 1/4, where `normal r1`, `normal
  r2` and `resume r1` all exhausted the 48-round tool budget with
  behavior/diff passing (`task.complete` refused; the completion-gate tail
  loop) and `resume r2` closed in 39 rounds. Behavior pass 9/12; provider
  healthy in every cell. Per M15_ACCEPTANCE §5 the valid FAIL **rejects the
  successor candidate**; the window is not rerun. M15 remains open.
- Serving availability (2026-09-03, recorded after the rerun): the PinAI
  direct endpoint now answers every POST (Responses and Chat) with 403
  "Access from this region requires trusted account access", and the
  localhost relay closes every forwarded POST at connection level while its
  GET `/models` still lists a larger catalog. No formal preflight or window
  can run until a serving is secured; the post-rerun diagnosis is banked at
  [`evidence/m15-diagnosis-successor-rerun/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-successor-rerun/REPORT.md).
- Serving restored 2026-09-04: the PinAI endpoint answers `/v1/responses`
  again (HTTP 200; the earlier 403 region block and the relay outage both
  lifted for the direct endpoint; the localhost relay stays broken and
  unused). By operator direction the M15 gate resumed on the current
  source: the exact-source product preflight **PASSED** on the clean tree
  `bf52490` (attempt `r1-attempt8` under `evidence/m15-preflight/`, source
  tree digest `86e36a2a...`): `retry_policy_dev` normal, product surface,
  behavior/diff pass, closure active (report-only), provider healthy,
  46 model rounds / 20 durable checkpoints, wall 496,317 ms.
- The ninth 12-cell v4 window on the bf52490 source + PinAI tuple is
  **predeclared 2026-09-04 before its run** (M15_ACCEPTANCE §7 item 8):
  3 fixtures × normal/resume × 2 repeats, the product surface, the pinned
  serving tuple (`gpt-5.6-luna`, Responses, 128,000-token context,
  4,096 max output tokens), one uninterrupted `agent-eval --m15-window`
  run. The mechanically regenerated report is the only accepted verdict;
  valid FAIL rejects the bf52490 candidate, and only a typed NOT_RUN
  permits a whole-window rerun.
- The semantic-completion-liveness candidate (committed with the ninth-
  window evidence at `050aa8e`) completed its pre-window exits: the
  complete workspace suite is green, Windows/Linux CI is green on
  `050aa8e`, and its exact-source preflight **PASSED** (attempt
  `r1-attempt9` under `evidence/m15-preflight/`, source tree digest
  `d6e7bfdd...`): `retry_policy_dev` normal, product surface,
  behavior/diff pass, closure active (report-only), provider healthy,
  11 model rounds, wall 348,355 ms.
- The tenth 12-cell v4 window on the `050aa8e` candidate + PinAI tuple is
  **predeclared 2026-09-04 before its run** (M15_ACCEPTANCE §7 item 8):
  3 fixtures × normal/resume × 2 repeats, the product surface, the pinned
  serving tuple, one uninterrupted `agent-eval --m15-window` run. The
  mechanically regenerated report is the only accepted verdict; valid
  FAIL rejects the candidate, and only a typed NOT_RUN permits a
  whole-window rerun.
- **M15 closed at its frozen gate 2026-09-04.** The tenth v4 window ran
  from the clean worktree at `a882789` and returned the mechanical verdict
  **PASS: 12/12, 0 NOT_RUN** (report `_windows/1788539149184`, committed):
  diag 4/4, migrate 4/4, policy 4/4 with three closure=completed cells,
  provider healthy everywhere, 212 rounds / max 34, wall max 562,563 ms.
  M15_ACCEPTANCE §4 closure conditions: every banked plane remains valid,
  the development plane passes, the bundles and generated report are
  committed, and no acceptance-path defect remains unresolved. All nine
  prior FAIL/censored windows stay immutable diagnostics. The post-M15
  order is `LT-EVAL-06` + the packaged V1 release gate, then the product
  phases in [`ROADMAP.md`](ROADMAP.md#route-to-a-usable-local-agent).
- Historical: ten v4 windows are banked (eight valid FAILs,
  one censored, one authorized rerun); the ninth window's 11/12 with 0
  NOT_RUN — policy 4/4 for the first time and
  a single stochastic diag overflow-edge miss. The two stochastic surfaces
  (diag `checked_shl` trap, policy completion-gate tail) have each passed
  4/4 in some window but never simultaneously; every infrastructure cause
  has been repaired and exonerated by the evidence.
- The successor-rerun diagnosis now identifies a Runtime-level convergence
  defect beneath that model behavior: raw call JSON was reused as result
  identity, advisory `next_action` could self-lock completion, volatile
  anchor/world revisions reset repair accounting, and the exact proof route
  could depend on prior model history. Repeated calls were therefore treated
  as fresh attempts even when the typed blocker frontier did not improve.
- A 2026-09-04 working-tree candidate implements semantic completion liveness:
  host-trusted effective-operation reconciliation, advisory-only `next_action`,
  `completion-repair.v2` episodes with typed progress/postconditions, a cold
  host declaration-to-exact-recipe route, and an audited text-only handoff when
  repair stalls. The handoff ends only the turn; the task remains active and no
  completion blocker is cleared automatically.
- This is candidate implementation, not formal evidence. Format and the
  selected package check/strict Clippy pass; `agent-runtime` library tests are
  340/340, Runtime turn integration tests 111/111, `tool-runtime` library tests
  214 pass with 1 ignored, and `agent-compose` library tests are 19/19. The
  complete workspace gate, dual-platform CI, a new exact-source preflight and a
  formal window remain separate exits. Historical v1 windows and their 6/12
  verdict are unchanged. M15 remains open.
- Product scope is now fixed to dynamic in-process Context, builtin tools, one
  workspace, one checked OpenAI-compatible provider profile and an interactive
  TUI. Service Context and dynamic extensions remain experimental unless their
  own typed-error and trust exits close.

| Module group | Current maturity | Product exit still required |
| --- | --- | --- |
| Contracts / Core / process / storage | Trusted mechanisms are substantial: bounded contracts, approval/effect authority, journals, recovery evidence and process control are implemented. | Persist and reconcile unresolved effect-ack debt end to end; retain fail-closed recovery. |
| Runtime / task / checkpoint | Sole `RuntimeActor`, TaskAnchor/ExecutionState, completion readiness, safe points and cross-plane checkpoint/restore exist. | Expose a verified product save/list/resume path and bounded status projection; never add a second orchestrator. |
| Dynamic Context | Working-set lifecycle, external store, recall and explainability are implemented; the unified plan/I/O mutation lane, catalog-dirty repair, exact-token Stored semantic verification and bounded fragment-aware short/CJK residual closed on 2026-09-04. | Keep GC/scoring frozen through M15; carry only measured post-M15 performance work, not new retrieval heuristics. |
| Tools / workspace | A broad bounded coding surface, artifact spill, revision-aware edits, verification recipes and approvals exist. | Persist the evaluated surface identity and make actual effects, failures and grants understandable in the product host. |
| Provider / composition | OpenAI-compatible Chat/Responses streaming and one trusted composition path exist. | Checked configuration, explicit mock mode, visible serving identity, strict startup errors and the optional-service type residual. |
| TUI / product entry | The event-driven TUI can run tasks, approve calls, inspect Context and manually checkpoint/restore. | Strict help/config parsing, bounded visible command errors, safe checkpoint-store integration, resume discovery, usable grant revoke and packaged startup smoke. |
| Eval / replay | Deterministic gates, doctor, replay and immutable formal evidence are stronger than the product shell. | Green clean-source CI, one materially new M15 candidate, then `LT-EVAL-06` breadth on the actual product limits. |
| Sidecars / extensions / multi-Agent | Protocol and supervision substrate exists; production isolation is incomplete. | Not in V1. Keep disabled/experimental; promote only through a separate measured gate. |

The product definition and phase exits live only in
[`ROADMAP.md`](ROADMAP.md#route-to-a-usable-local-agent). Open defect details
live only in [`AUDIT_TODO.md`](AUDIT_TODO.md#live-execution-queue--2026-09-04).

**Continuation-review verification (2026-09-03; audited source
`c823a1c2641099ccd42517c4ec96c6ebbe2ca953` against
`ea8deefc873abee13106de92bbbb3ddbaeb2d423`):**

- The supplied review synopsis was checked against the current code, tests,
  committed evidence and git history; its transient `sandbox:/mnt/data/...`
  appendix is not treated as repository evidence. The worktree was clean and
  `main` matched `origin/main` at the audited source.
- The local source gate was rerun: `cargo fmt --all -- --check`,
  `cargo check --workspace --all-targets --all-features`, strict all-target /
  all-feature Clippy, `cargo build --workspace --all-targets`, and the complete
  all-target workspace suite all pass. On this Windows host the test command
  needs the bundled Python 3.12 ahead of the Windows Store `python.exe` stub;
  the uncorrected stub produced only exit `9009`, while the corrected full run
  passed.
- The review's high-level product diagnosis is accepted: the trusted Runtime,
  Context, effect and recovery substrate is substantially formed, while a
  cross-run queryable execution projection and the product operation surface
  remain post-M15 work. Several proposed "next" repairs were already landed;
  the exact remaining seams and closure records are reconciled in
  [`AUDIT_TODO.md`](AUDIT_TODO.md#continuation-review-verification--2026-09-03-c823a1c).
- This review does not change the frozen M15 experiment: formal M15 remains
  TaskProgress-on / settlement-off, 3 fixtures × normal/resume × 2 repeats.
  At the audited `c823a1c` boundary the governing relay preflight was a typed
  `NOT_RUN`; the later PinAI preflight and valid FAIL window recorded below
  supersede that gate state without changing this review's scope. M15 remains
  open and no TaskGraph, worker or Self-Iteration work is authorized by the
  review.

**Repository-wide P0/P1 repair tranche (2026-08-31 through 2026-09-02;
recorded source `615b5ed..6fdb4f0`; dual-platform CI green on `6fdb4f0`,
run `33624084700`, 2026-09-02):**

- Every selected-path P0 from the 2026-08-31 repository audit now has a
  landed repair: typed effect-ack settlements with journal v2 and
  no-strengthening recovery (`6112ffd`); Runtime-owned turn-start/checkpoint
  Context transactions with rollback fencing and scratch-state restore
  validation (`9ba85d3`, `f42a898`, `f622cf3`); fsynced event-flush barriers
  and preflighted WAL compaction (`f055e39`); ExecArgv grants bound to the
  resolved executable identity with a pre-spawn seal recheck (`f460558`,
  `13cf6c1`); Landlock write-floor attestation gated to enforcing ABIs
  (`e5e712f`); process.session capacity/cancel/kill-then-reap enforcement
  (`64607f6`); and content-addressed M15 cell inputs with fail-closed
  mutation tests (`ea821bb`).
- All ten audited P1 implementation repairs landed: materialization
  validation before provider execution (`d9807e7`), authoritative blob
  checksums plus session-boundary reconcile (`1ea671f`), consumption ledger
  from the final rendered frame (`7a8a663`), allocation-time GC/report bounds
  (`cfc17a3`), live-input/task/event/replay byte bounds (`ab27f86`), the
  bounded effect-coordinator wire protocol (`43eb87b`), capability lifecycle
  settlement (`abcb4ba`), process-hygiene abort/drain bounds (`ebe02ff`),
  the declared layer/role graph enforced as an allowlist (`2436249`), and
  bounded failure-monotone eval cells (`f57a118`).
- Post-window execution items closed on this source: the durable
  completion-repair stage with criterion details and the atomic
  proof-refresh transaction (`615b5ed`, `b148b4d`, `d92b250`, regressions
  `b554058`, `8dfa452`, `93e434a`, `7026d8a`); Responses/Chat terminal-state
  hardening against identity and snapshot drift (`1768914`); typed retry
  records with call identity and terminal stages (`f528a92`); SSE event-name
  versus payload-type fail-closed routing (`328ec5d`); pre-approval schema
  validation against compiled round surfaces (`33d0395`); whole-record
  edge-triggered TaskProgress packing (`531a77e`, `df2f72c`); the production
  tool inventory aligned and fail-closed (`23abe1c`); restored protocol
  bodies accounted apart from selected-context cost (`83cbd60`, actor
  regression committed on `e357bed`).
- Governance: `GOV-MAINT-01` is closed — `LICENSE` (MIT) and a minimal
  `inspect_outbound.sh` landed in `6fdb4f0`. `GOV-STATUS-01` is resolved in
  this documentation tranche (2026-09-03): the AGENTS.md M12/M13 wording is
  reconciled to one suspended-claims statement and README is aligned
  (renamed crate, complete crate list, historical `CONTEXT_RUNTIME_TODO.md`,
  authority pointers); it closed on `bba1c76`.
- The repaired-candidate gate sequence advanced after this tranche. By user
  decision (2026-09-03) the serving moved from the
  broken localhost relay back to the original PinAI tuple
  (`https://api.pinaic.com/v1`, `gpt-5.6-luna`, Responses, explicit
  protocol, 128,000-token context, 4,096 max output tokens); the
  exact-source product preflight on that tuple **PASSED** the same day on
  the clean tree (attempt `r1-attempt5` under `evidence/m15-preflight/`,
  source tree digest `9437882d...`, serving recorded in the attempt
  manifest): `retry_policy_dev` normal, product surface, behavior/diff
  pass, closure completed, provider healthy, 16 model rounds / 39 tool
  calls / 0 failed outputs, wall 192,596 ms, hidden oracle green. The
  earlier relay attempt is a retained NOT_RUN (see below);
  `--m15-preflight --evidence-dir` was not honored by the subcommand, so
  the attempts share the directory and are attributed by their per-attempt
  serving manifests. All other implementation exits are
  recorded: the M10 fault-gate re-audit (2026-09-03,
  `RUNTIME-CONTEXT-COMMIT-01` in [`AUDIT_TODO.md`](AUDIT_TODO.md)),
  `GOV-STATUS-01` (`bba1c76`), the actor protocol-body regression
  (`e357bed`), the formal-path/product retry observer (`7e02488`), and one
  recorded clean source with local plus dual-platform CI green (`4e56f69`
  code, run `33663057012`). The historical FAIL packs stay immutable and
  Context/GC/retrieval/packing remain frozen.
- The clean-source P0-close exit is banked 2026-09-03 on `97a7719` (the
  recorded attempt-incident candidate FAIL source): full local gate green —
  fmt, all-target build, strict all-feature Clippy and the complete
  all-target workspace suite (`TEST_EXIT=0`) — plus dual-platform CI run
  `33709924715` green on the exact source. Every open P0 heading in
  [`AUDIT_TODO.md`](AUDIT_TODO.md) closed on that source; the remaining
  operational gate is the M15 sequence itself.
- The 12-cell v4 window on the PinAI tuple was **predeclared 2026-09-03
  before its run** (M15_ACCEPTANCE §7 item 8): 3 fixtures × normal/resume ×
  2 repeats, the product surface (TaskProgress on, settlement and advisory
  candidates off, no counterfactual second request), the pinned serving
  tuple above, one uninterrupted `agent-eval --m15-window` run whose cell
  directories land under
  `crates/agent-eval/evidence/m15-window/_windows/<timestamp>/`. The exact
  clean source identity is recorded at launch; no source change happens
  during the run, the frozen-window rules of M15_ACCEPTANCE §5 apply, and
  the mechanically regenerated report is the only accepted verdict. Valid
  FAIL rejects the candidate; only a typed NOT_RUN permits a whole-window
  rerun.
- The 12-cell v4 window on the PinAI tuple ran 2026-09-03 on the
  predeclared clean source `43e1033` (cell-recorded source tree digest
  `fd9799c9...`) and is a **valid FAIL: 6/12 pass, 0 NOT_RUN** — the
  mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788385151733/`. Migrate
  4/4 (all ~10–11 rounds); diag 1/4 (normal r1/r2 fail fast on the
  overflow-edge needle, resume r2 fail after 47 rounds; resume r1 pass);
  policy 1/4 — normal r1 exhausted the 48-round tool budget with
  behavior/diff passing (the completion-gate compliance loop previously
  diagnosed at `784d7aa`: `edit.patch` 7 failed of 14, closure refused),
  resume r1 died on a new malformed-event surface (the model's buffered
  stream exceeded the bounded-framer chunk cap, 16,385/16,384 — the guard
  worked as designed and is non-retryable), and resume r2 exhausted the
  48-round budget in phase two (`task.complete` refused 6/6, 15
  `verify.run` calls, 125 tool calls). Provider healthy in every cell;
  behavior pass 8/12; rounds 243 total / max 53; provider input 2,441,871 /
  output 81,827 (cached 217,600). Per M15_ACCEPTANCE §5 the valid FAIL
  rejects the repaired-source + PinAI-luna candidate and returns to
  diagnosis; the window is not rerun. M15 remains open; candidate selection
  is a user decision. The relay NOT_RUN preflight and the preflight PASS
  attempt remain retained evidence.
- Post-window diagnosis recorded 2026-09-03 at
  [`evidence/m15-diagnosis-repaired-source/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-repaired-source/REPORT.md),
  read entirely from the immutable cell streams. The three failure
  observations stay distinct with no shared cause: (1) all three failing
  diag cells wrote the `checked_shl` trap (the passing cell wrote
  `checked_mul`) — the same solver limitation attributed across the v3
  windows, now on a third serving; (2) both policy 48-round tails are the
  `784d7aa` completion-gate loop recurring with the completion-repair stage
  live — in `normal r1` the persistent blocker is one `process.run` exit=1
  plus patch churn that re-opens failed-command rows and re-stales
  verification, and the loop is stochastic (`normal r2` closed in 20
  rounds); (3) the framer malformed-event is a first-occurrence runaway
  delta stream (16,385/16,384 chunks) where the bound failed closed as
  designed. No harness, transport or oracle defect in any class; the
  deterministic gate chain is fully green on this source, so the remaining
  surface is per-cell model behavior. Candidate selection is a user
  decision bounded by the frozen route (no Context/GC retune, no protocol
  weakening, no round stop, no TaskGraph, no prompt pressure).

**Attempt-incident admission candidate window (2026-09-03; source `38d458e`,
preflight PASS and predeclaration recorded in that commit):**

- The 12-cell v4 window on the attempt-incident candidate + PinAI tuple is
  **predeclared 2026-09-03 before its run** (M15_ACCEPTANCE §7 item 8):
  3 fixtures × normal/resume × 2 repeats, the product surface (TaskProgress
  on, settlement and advisory candidates off, no counterfactual second
  request), the pinned serving tuple above, one uninterrupted
  `agent-eval --m15-window` run whose cell directories land under
  `crates/agent-eval/evidence/m15-window/_windows/1788402676712/`. The
  exact clean source identity is recorded at launch; no source change
  happens during the run, the frozen-window rules of M15_ACCEPTANCE §5
  apply, and the mechanically regenerated report is the only accepted
  verdict.
- The 12-cell window ran on the predeclared clean source `38d458e`
  (cell-recorded source tree digest `0cecc539...`) and is a **valid FAIL:
  10/12 pass, 0 NOT_RUN** — the mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788402676712/`. Migrate
  4/4 (7–19 rounds, clean continuation everywhere resumed); diag 3/4;
  policy 3/4. Behavior and allowed-diff pass 12/12; provider healthy in
  every cell; closures 3/12 (all three policy cells that closed used
  `task.complete`). The two failures are `retry_diag_dev normal r2`
  (48-round phase-one budget; `task.complete` refused 18/18 over
  `acceptance_undeclared` + `operator_closure_only` + later next-action)
  and `retry_policy_dev resume r1` (48-round phase-two budget;
  `task.complete` refused 12/12 over verification-currentness /
  acceptance-coverage / open-loop / next-action blockers). Both failing
  cells carry functionally-correct workspaces whose injected oracle tests
  pass — the `checked_shl` diag trap did not recur (the failing diag cell
  wrote a correct `checked_mul`/`min(63)` shape and missed only the static
  `u128`/`leading_zeros` marker), and the 2026-08-31 P1 admission guarantee
  held (policy refusals never cite failed-command debt). Per
  M15_ACCEPTANCE §5 the valid FAIL rejects the candidate and returns to
  diagnosis; the window is not rerun. Post-window diagnosis at
  [`evidence/m15-diagnosis-attempt-incident/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-attempt-incident/REPORT.md).
  M15 remains open; candidate selection is a user decision.

**Repository-wide architecture audit (2026-08-31; started at `c8b9dbb`,
incrementally reviewed through `c55429c`; findings now repaired by the
tranche above):**

- Seven new P0s are confirmed: typed effect receipts are collapsed to a bool
  across broker ACK recovery; turn-start/checkpoint Context mutations are not
  one Runtime-owned recoverable transaction; advertised durable event/WAL
  barriers have fsync/compaction gaps; process standing grants omit resolved
  executable/cwd/environment identity; old Landlock ABIs over-attest truncate
  confinement; process sessions do not fully enforce cancel/capacity/reap; and
  the M15 reporter trusts derived files instead of rebuilding content-addressed
  raw cell evidence.
- P1 findings cover complete Context adapter validation, store checksum/startup
  reconcile, exact post-pack consumption truth, GC/report allocation bounds,
  live-input/task/event/replay limits, capability/process supervision, the
  declared dependency graph/conformance guard, and bounded failure-monotone M15
  harness execution. Exact evidence and exits are in the 2026-08-31 tranche of
  [`AUDIT_TODO.md`](AUDIT_TODO.md).
- The findings do not reinterpret immutable historical FAIL packs or authorize
  Context/GC retuning. They do invalidate a new product preflight/formal M15
  window until `M15-RAW-EVIDENCE-01` and every selected-path P0 close; a
  conditional/non-selected P0 needs exact source/surface/OS exclusion evidence.
  `RUNTIME-CONTEXT-COMMIT-01` requires the M10 runtime-consistency gate to be
  re-audited rather than assumed from happy-path tests.

**Merged audit (2026-08-30; recorded source `a3bd23f` plus the
`BASELINE-01` commit chain; not an evidence source by itself):**

- P0 candidate code now derives one `CompletionReadiness`, preserves the
  directive epoch on `TaskContinuation`, mints only post-PASS criterion
  receipts bound to the current host coverage declaration, resolves failures
  by typed identity/domain with fail-closed overflow, and makes required
  Context misses completion-visible. Required-body overlay first proves a
  displacement feasible and only then commits it, so an oversized mandatory
  body records a miss without destroying the useful optional frame.
- Task closure now uses a prospective terminal checkpoint and explicit
  `RuntimeCommitBarrier`. New traces start with a durable format marker, replay
  rebuilds only the committed prefix, and a validated terminal checkpoint is
  stronger truth in the checkpoint-to-audit crash window.
- Runtime startup is one-shot: only a successfully flushed `RunStarted +
  RuntimeCommitBarrier(RunStart)` batch enters `Serving`; a partial append or
  flush failure enters `StartFailed`, rejects later mutation/retry and writes no
  synthetic shutdown completion.
- Provider parsing now fails closed on malformed SSE/known events/tool
  arguments and missing terminal markers. Buffered eval retry is bounded; a
  live sink never replays already published output.
- Eval now has independent task-progress/settlement switches, stable pair
  identity, bounded real-order episode records and a harness-verified
  same-state request audit. The product default path performs neither the
  counterfactual second input nor its request hashing.
- Live settlement causality is a conditional gap: if the selected candidate
  enables settlement, both arms must fork from one pre-exposure durable
  checkpoint and byte-identical workspace while preserving opaque ids and an
  explicitly pinned provider protocol. A settlement-off base skips this live
  pair. The historical
  convergence bundle remains mechanical FAIL and causally
  **INVALID/CONFOUNDED**; it is not reinterpreted.
- `BASELINE-01` closed 2026-08-30 on recorded source `1455795` (Rust tree
  `8558886`): the four local commands pass, and Ubuntu and Windows CI are
  both green on one complete run. The last open Ubuntu exit was a
  hosted-runner loss of communication at ~48 minutes of job wall time, twice
  at nearly identical timestamps (48m02s / 47m59s from job start) with the
  test step in progress and zero test-level failures — a wall-clock
  termination, not a test result — so the CI test step was split into per-OS
  parts in separate fresh-VM jobs: Ubuntu runs two halves sized from measured
  per-crate test durations (3m09s + 4m49s) and Windows keeps the full
  workspace (15m01s). The scoped test jobs exposed and fixed three fixtures
  the all-members layout had masked (the Unix process-group kill contract in
  the persist tests, missing sibling binaries in the part-2 / full jobs, and
  the context-service binary mtime guard against warm-cache restore).
- `VERIFY-ROUTE-01` closed 2026-08-30 on the deterministic verify-route gate:
  the `verify.run` schema catalog marks each recipe's identity class and
  declared coverage domain, the acceptance-criterion view line names the
  required domain, and the three-cell gate
  (`crates/agent-eval/evidence/verify-route/`) proves a broad task-scoped
  PASS never mints a receipt (completion refused until the declared exact
  recipe runs), the first exact PASS satisfies and identical repeats reuse
  it, and an unrelated failed command survives and keeps completion refused.
  Evidence is event-derived, not fixture-derived; Context/GC/retrieval/
  packing are untouched. Closed on commit `7ee56e8` with the four local
  commands green and dual-platform CI green (run `33305302134`: Ubuntu
  parts 1+2 and the Windows full suite).
- M15 exact-source/product preflight passed 2026-08-30 on commit `df85195`
  (clean head; source tree digest `6ae2509007ee225e...`): one
  `retry_policy_dev` normal cell with the product surface (TaskProgress on,
  settlement and advisory candidates off, no counterfactual second request)
  and the unswitched pinned serving tuple with an explicit protocol
  completed cleanly — behavior/diff pass, closure completed, provider
  healthy, 32 model rounds / 76 tool calls, no retryable transport outcome.
  Evidence: `crates/agent-eval/evidence/m15-preflight/`. The single
  predeclared 12-cell v4 window (M15_ACCEPTANCE §2) is next and has not
  been run; the serving tuple stays pinned without fallback.
- The 12-cell v4 window is **predeclared 2026-08-30 before the run**:
  3 fixtures × normal/resume × 2 repeats, the product surface, the pinned
  serving tuple (PinAI `/v1`, `gpt-5.6-luna`, Responses, explicit
  protocol), one uninterrupted `agent-eval --m15-window` run whose cell
  directories land under
  `crates/agent-eval/evidence/m15-window/_windows/<timestamp>/`. The exact
  clean source identity is recorded at launch per M15_ACCEPTANCE §7
  item 8; no source change happens during the run, the frozen-window rules
  of M15_ACCEPTANCE §5 apply, and the mechanically regenerated report is
  the only accepted verdict.
- The 12-cell v4 window ran 2026-08-30 on the predeclared clean source
  `d1936d4` (product surface, pinned serving, explicit protocol) and is a
  **valid FAIL: 9/12 pass, 0 NOT_RUN** — the mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788093162603/`. Migrate
  4/4; policy 3/4 (normal r2 exhausted the 48-round tool budget before
  closure); strategi against the frozen saturate-not-wrap oracle. Per
  M15_ACCEPTANCE §5 the valid FAIL rejects the current base candidate and
  returns to diagnosis; the window is not rerun. Post-window diagnosis at
  `crates/agent-eval/evidence/m15-diagnosis/REPORT.md` closes both
  mechanisms from the immutable cell streams (diag: `checked_shl`
  truncates overflow to zero — `100u64 << 62` → 0 — against the frozen
  `u128`-widening marker; policy: completion refused on a stale
  verification basis plus one unresolved failed command, then no re-proposal
  and budget exhaustion). No harness/transport/oracle defect; candidate
  selection is a user decision.
- The serving/model candidate switched by user decision (2026-08-30) to the
  localhost OpenCode relay tuple (`http://127.0.0.1:8787/v1`,
  `deepseek-v4-flash`, Responses, 128k context, 16,384 max output tokens).
  Its preflight chain surfaced one harness defect, fixed in `provider-openai`
  (commit `a242736`, CI run `33319082971` green): a model call recorded raw
  while its tool was not yet exposed (e.g. `fs_mkdir` for `fs.mkdir`) stayed
  in history, and the per-request wire-name codec failed closed once the spec
  became exposed. Spec mappings are now authoritative and colliding history
  wire names are that tool's wire form (skipped, not errors); spec-vs-spec
  collisions still fail closed.
- The relay exact-source/product preflight passed 2026-08-30 on commit
  `a242736` (clean head; source tree digest `f8b57b46a3e56c49...`): one
  `retry_policy_dev` normal cell with the product surface (TaskProgress on,
  settlement and advisory candidates off, no counterfactual second request)
  and the relay serving tuple with an explicit protocol completed cleanly —
  behavior/diff pass, closure completed, provider healthy, 23 model rounds /
  35 tool calls, no retryable transport outcome. Evidence:
  `crates/agent-eval/evidence/m15-preflight-relay/` (the earlier failed
  attempts are retained: Chat SSE shape, 4,096-token output truncation, and
  the wire-name collision above).
- The 12-cell v4 window on the relay tuple is **predeclared 2026-08-30
  before the run**: 3 fixtures × normal/resume × 2 repeats, the product
  surface, the relay serving tuple above with an explicit protocol, one
  uninterrupted `agent-eval --m15-window` run whose cell directories land
  under `crates/agent-eval/evidence/m15-window/_windows/<timestamp>/`. The
  exact clean source identity is recorded at launch per M15_ACCEPTANCE §7
  item 8; no source change happens during the run, the frozen-window rules
  of M15_ACCEPTANCE §5 apply, and the mechanically regenerated report is the
  only accepted verdict.
- The 12-cell v4 window on the relay tuple ran 2026-08-30 on the predeclared
  clean source `a25a8a5` (product surface, relay serving, explicit protocol)
  and is a **valid FAIL: 10/12 pass, 0 NOT_RUN** — the mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788105967425/`. Diag 4/4
  and migrate 4/4; policy 2/4, its two failures resolved into two
  mechanisms (the attribution is corrected by the 32,768 window below):
  `normal r2` died on the explicit output-limit error, while `resume r2`'s
  ~8 KB malformed tool-call argument is the same model wire-quality
  weakness that recurs independently of the cap. Per M15_ACCEPTANCE §5 the
  valid FAIL rejects the relay candidate and returns to diagnosis at
  `crates/agent-eval/evidence/m15-diagnosis-relay/REPORT.md`; the window is
  not rerun. M15 remains open; candidate selection is a user decision.
- By user decision (2026-08-30) the same relay model was re-pinned with
  32,768 max output tokens. A bounded probe established the tuple can honor
  the cap: the upstream emitted 22,341 output tokens in one response without
  truncation.
- The 32,768-tuple exact-source/product preflight passed 2026-08-30 on
  commit `f32f22d` (clean head; source tree digest `16d97ccb81696f8b...`):
  one `retry_policy_dev` normal cell with the product surface (TaskProgress
  on, settlement and advisory candidates off, no counterfactual second
  request) completed cleanly — behavior/diff pass, closure completed,
  provider healthy, 17 model rounds / 33 tool calls. Evidence:
  `crates/agent-eval/evidence/m15-preflight-relay-32768/`.
- The 12-cell v4 window on the 32,768-tuple is **predeclared 2026-08-30
  before the run**: 3 fixtures × normal/resume × 2 repeats, the product
  surface, the relay serving tuple with an explicit protocol and 32,768 max
  output tokens, one uninterrupted `agent-eval --m15-window` run whose cell
  directories land under
  `crates/agent-eval/evidence/m15-window/_windows/<timestamp>/`. The exact
  clean source identity is recorded at launch per M15_ACCEPTANCE §7 item 8;
  no source change happens during the run, the frozen-window rules of
  M15_ACCEPTANCE §5 apply, and the mechanically regenerated report is the
  only accepted verdict.
- The 12-cell v4 window on the 32,768-tuple ran 2026-08-30 on the
  predeclared clean source `ab4534a` (product surface, relay serving,
  explicit protocol, 32,768 max output tokens) and is a **valid FAIL:
  9/12 pass, 0 NOT_RUN** — the mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788109477415/`. Diag
  3/4 and migrate 4/4; policy 2/4. All three failures are
  `malformed-tool-call` at argument columns far below either output cap
  (521 / 10,526 / 10,736 characters): the model emitted tool-call argument
  JSON that ends prematurely (EOF mid-list) or breaks JSON syntax,
  rejected fail-closed by the provider's strict parser. This corrects the
  16,384-window attribution: only `policy normal r2`'s explicit
  output-limit error was cap-bound; malformed arguments recur
  independently of the cap (diagnosis at
  `crates/agent-eval/evidence/m15-diagnosis-relay-32768/REPORT.md`). Per
  M15_ACCEPTANCE §5 the valid FAIL rejects the 32,768 relay tuple; the
  window is not rerun. M15 remains open; candidate selection is a user
  decision.
- Model-candidate probes (2026-08-31): the relay serves only
  `deepseek-v4-flash`, `deepseek-v4-pro` and `grok-4.5` on the Responses
  endpoint (the protocol the harness pins); the Qwen models
  (`qwen3.8-max`, `qwen3.7-max`, ...) exist on the relay but return 401
  "not supported for format openai", and glm/kimi/hy3/mimo answer 501
  (Responses unavailable). By user decision the route became: solve the
  intermittent malformed tool-call problem at the harness rather than
  switching models.
- Harness fix (commit `41f06ad`, CI `33325617880` green, 2026-08-31):
  that historical source made the default system prompt require every tool
  call argument to be
  one complete valid JSON value, and `provider-openai` classifies the
  model-emitted `MalformedToolCall` (argument JSON that ends prematurely
  or breaks JSON syntax, at columns far below the output cap) as
  retryable: the eval's buffering transport re-issues the request from
  scratch, never leaks the rejected stream into the sink, and is bounded,
  so persistent malformed output still fails honestly with the attempt
  count recorded. Wire damage (`MalformedEvent`) stays non-retryable and
  interactive (live) hosts never replay emitted deltas.
- The re-pinned exact-source/product preflight passed 2026-08-31 on
  commit `41f06ad` (clean head; source tree digest `d4b4da3517f7a3e8...`):
  one `retry_policy_dev` normal cell with the product surface (TaskProgress
  on, settlement and advisory candidates off, no counterfactual second
  request) completed cleanly — behavior/diff pass, closure completed,
  provider healthy, 16 model rounds / 27 tool calls. Evidence:
  `crates/agent-eval/evidence/m15-preflight-relay-fix/`.
- The 12-cell v4 window on the fixed source is **predeclared 2026-08-31
  before the run**: 3 fixtures × normal/resume × 2 repeats, the product
  surface, the relay serving tuple (v4Flash, Responses, 32,768 max output
  tokens), one uninterrupted `agent-eval --m15-window` run whose cell
  directories land under
  `crates/agent-eval/evidence/m15-window/_windows/<timestamp>/`. The exact
  clean source identity is recorded at launch per M15_ACCEPTANCE §7 item 8;
  no source change happens during the run, the frozen-window rules of
  M15_ACCEPTANCE §5 apply, and the mechanically regenerated report is the
  only accepted verdict.
- The 12-cell v4 window on the fixed source ran 2026-08-31 on the
  predeclared clean source `784d7aa` (product surface, relay serving,
  explicit protocol, 32,768 max output tokens) and is a **valid FAIL:
  10/12 pass, 0 NOT_RUN** — the mechanical report at
  `crates/agent-eval/evidence/m15-window/_windows/1788115951355/`. The
  malformed-tool-call failure mode did not recur: behavior pass 12/12,
  provider healthy in every cell, no malformed-JSON outcome in any
  summary. The new retry path was exercised twice (`retry_migrate_dev
  resume r2` and `retry_policy_dev resume r1`) and both cells passed —
  one `model_used` event records 2 attempts / 1 retry. The two failures
  are `retry_policy_dev normal r1` and `r2`, both erroring "phase one
  failed: tool round budget exhausted after 48 rounds" with
  `task.complete` refused 3/3 and 5/5 and no retries: the model failed
  to close these cells (the same cells that failed via malformed
  arguments on `ab4534a`), so the fix removed its target failure mode
  and exposed a model task-execution failure on this fixture. Per
  M15_ACCEPTANCE §5 the valid FAIL rejects the candidate; the window is
  not rerun. M15 remains open.
- Post-window diagnosis (2026-08-31,
  `crates/agent-eval/evidence/m15-diagnosis-closure-gate/REPORT.md`):
  both failures are completion-gate compliance, not transport/JSON
  defects. The model completed the functional task (final workspace
  satisfies the directive; `verify.run` green; its 15 tests pass) but
  its last trusted verification went stale after three successful
  post-verify `shell.exec` runs (`cargo test`, `cargo fmt --check`,
  `cargo clippy`), early fail-closed tool-name calls (`shell.exec`
  before `capability.manage` load, `fs_mkdir`/`shell_exec` wire
  variants) left permanently unresolved failed-command rows in the
  execution ledger, and it could not act on the refusal messages,
  exhausting the 48-round budget. The oracle hidden checks bind
  implementation detail (needle text) and flag three equivalently
  correct implementations false. Passing cells differ behaviorally:
  tools loaded before first use, a current `verify.run` as the final
  action. Any harness-visible candidate fix requires deterministic
  gates plus an exact-source preflight and a fresh predeclared window.
- POST-fix observation upgrade (2026-08-31): `provider-openai` retry
  loops no longer swallow the first-attempt error. All three retry paths
  (non-streaming `retry()`, live streaming, buffered streaming) write one
  stderr line on each retry with the reason class
  (`malformed tool-call JSON` / `retryable transport error`), attempt,
  and delay; provider error bodies are deliberately omitted. The eval harness
  redirects stderr into the run log, so a future window/preflight will record
  which error class triggered each retry without copying intermediate provider
  content. Typed durable retry evidence remains open. No window or preflight
  is rerun on this change.
- Current repair checkpoint (2026-08-31, recorded in `7dc9f46` with the actor
  regression in `c8b9dbb`; not live evidence): off-surface command proposals are now typed
  `SurfaceUnavailable` attempt incidents and remain visible without entering
  Context or completion debt. A refused completion returns bounded
  `completion-repair.v1` single-stage snapshot stamped with the current
  task/verification/world basis. Runtime re-derives the current blocker stage
  each decision, replaces the bounded `TASK PROGRESS` repair record and only
  prefers its resolver; a repair helper cannot abort a model round when
  loading or packing fails. The standing `task.complete` control remains
  model-visible, so an explicit re-proposal can also refresh the stage.
  This directly targets the two 48-round policy-normal loops without changing
  Context/GC, adding a round TTL or reducing model autonomy.
  This paragraph is retained as the v1 historical record; the current
  semantic-episode contract is `completion-repair.v2` above.
- JSON recovery is now a protocol mechanism rather than standing prompt text:
  the global JSON sentence is removed, Runtime's live sink declares tool-call
  deltas internal until text is published, and malformed tool arguments receive
  one immediate format regeneration independent of network retry/backoff.
  Targeted Provider, Runtime and contract tests are green. The complete local
  all-target workspace suite is also green after rebuilding the freshness-
  guarded context service binary (all tests passed; the existing 10k-turn
  bounded-working-set test took about 128 seconds in the final run). Format,
  strict all-target/all-feature Clippy, all-target build, full workspace tests
  and diff check were green for that candidate; this is not yet the complete
  clean-source/dual-CI or live-evidence exit. `dfb9ade` later added the bounded
  SSE framer and protocol state candidate; `c55429c` repaired formatting and
  added diagnostic typed retry records. Responses terminal/identity conflicts,
  formal-path complete retry evidence and immutable-round schema validation
  remain open in `AUDIT_TODO.md`.
- M15 remains open. Its shape remains 3 fixtures × normal/resume × 2 repeats =
  12 cells. Historical v3 FAIL windows remain immutable; six v4 valid FAILs
  are banked: 9/12 on `d1936d4`, 10/12 on `a25a8a5`, 9/12 on `ab4534a`,
  10/12 on `784d7aa`, 6/12 on `43e1033`
  (`_windows/1788385151733`, 0 NOT_RUN), and 10/12 on `38d458e`
  (`_windows/1788402676712`, 0 NOT_RUN). No v4 window has passed.
  `GOV-STATUS-01` closed on `bba1c76`; M12/M13 claims remain suspended by
  the open M15 gate, so Self-Iteration remains blocked.

### Historical evidence chronology (non-authoritative)

The dated observations below remain useful evidence but do not override the
snapshot, merged TODO or ordered route above.

- M11 and M14 are closed at their named gates. M10 was historically recorded
  closed, but the 2026-08-31 `RUNTIME-CONTEXT-COMMIT-01` evidence supersedes
  that claim until its fault gate is re-audited.
- Context V1 operational core and **Execution Coherence V1 are both
  freeze candidates**: the 2026-08-23 long-flow pass confirmed the
  coherence machinery (MOD-OBS-01 observation, MOD-PROG-01 stall,
  turn checkpointing) held; Warm=Stored rereads stayed 0. Do not retune
  or extend them as product work.
- Provider routing is now explicit and isolated. PinAI is a direct external
  provider using `/v1/responses`; the localhost OpenCode relay is a separate
  base URL and may use its own direct-then-proxy upstream policy. There is no
  PinAI -> localhost or cross-provider fallback. `provider-openai` supports
  bounded Responses SSE/tool continuation plus Chat compatibility, caches only
  same-base unsupported-protocol negotiation, and treats streamed
  `network_error` as a retryable failure instead of an empty completion. This
  improves live-gate validity but is not evidence that the convergence gate is
  closed. The 2026-08-24 post-fix short gates kept the routes separate and both
  passed hidden `add_test`: direct PinAI Responses with `gpt-5.6-luna` used 7
  model rounds / 6 tool calls, while localhost OpenCode `ox-alpha-free` used
  same-base `auto` negotiation and 8 rounds / 7 calls with zero failed tool
  outputs. Both committed their edit on the first attempt. These are transport
  and tool-loop smoke results, not paired long-flow or convergence evidence.
  OpenCode Muse 1.2 remains account-gated by an explicit data-contribution
  opt-in response; proxy fallback correctly does not retry that non-region
  authorization decision. See
  [`provider-routing-smoke-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/provider-routing-smoke-2026-08-24/REPORT.md).
- The subsequent four-cell Tool Edit diagnostic separated editor correctness
  from provider availability. Direct PinAI/Luna passed strict hidden and edit
  gates 4/4 in 15 rounds with zero failed tool outputs; every canonical patch
  committed on its first valid attempt, including CRLF, mixed-EOL, stale-read,
  and two-file cases. Local OX scored strict 2/4 and gate 0/4 initially because
  3 cells hit streamed `network_error`; a bounded route-health/session-rotation
  relay change did not improve a second run because both direct and system-
  proxy paths received the same upstream failure. Do not attribute those OX
  failures to Context or `edit.patch`, and do not use OX for acceptance until
  an independent availability smoke is green. These dirty-tree runs are
  diagnostic only; see
  [`provider-routing-tool-edit-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/provider-routing-tool-edit-2026-08-24/REPORT.md).
- A direct PinAI/Luna Responses `late_constraint_long` A/C diagnostic then
  passed hidden verification in both arms and confirmed that C's Context
  advantage remains large: model input -34%, selected resident/reactivated
  tokens -87%, historical-context tokens -80%, and final resident bytes -98%
  versus A. Execution still amplified to 75/85 rounds/calls versus A's 65/57,
  with 29 versus 9 failed outputs. Twenty-five C failures were malformed
  pagination capabilities. Treating empty/zero cursors as page one was tested
  and rejected: C grew to 137 rounds / 171 calls, max-turn rounds rose 12→47,
  and the paired A cell timed out. A follow-up strict-schema run also failed:
  C passed hidden but used 107 rounds / 112 calls, while A timed out at turn 6;
  the model fabricated regex-shaped artifact identities and all 25
  `fs.list`/`search.grep` calls failed. The retained design exposes only one
  model continuation surface: first-page tools return a bounded view plus
  `artifact_ref`, and `artifact.read` reads further lines; legacy per-tool
  cursors remain parser-only. The same traces exposed a separate
  `context.manage` union-parser defect, now corrected by parsing only fields
  relevant to the selected op while keeping required/relevant values strict;
  its follow-up calls passed 4/4. Context selection, retrieval budgets, GC,
  autonomy, and packing are unchanged. See the retained
  [`baseline report`](../crates/agent-eval/evidence/longflow-pinai-luna-responses-2026-08-24/REPORT.md)
  and the
  [`negative experiment`](../crates/agent-eval/evidence/longflow-pinai-luna-cursor-normalized-2026-08-24/REPORT.md),
  plus the
  [`strict-schema follow-up`](../crates/agent-eval/evidence/longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md).
  That follow-up also isolated five failed `task.complete` calls caused by
  model-echoed artifact claims. Runtime already attaches current assistant and
  verification evidence at completion, so the artifact list is now
  parser-only compatibility and the model supplies only the bounded summary.
  A retained follow-up pair passed 4/4 in both arms with only 2/1 failures;
  C still used 77 rounds / 84 calls versus A's 49 / 36, proving failure
  cleanup alone does not close convergence. See
  [`longflow-pinai-luna-unified-artifacts-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pinai-luna-unified-artifacts-2026-08-24/REPORT.md).
  `task.complete` is now terminal at the same durable safe point when its
  entire sibling batch succeeds and current verification remains valid; a
  failed sibling still gets another model recovery decision. The live trace
  proved the confirmation round disappeared, but also exposed the deeper
  loop: C closed the durable task on 9/15 turns versus A's 3/15, clearing
  task affinity and causing the next directive to rediscover tools/files.
  See
  [`longflow-pinai-luna-terminal-completion-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pinai-luna-terminal-completion-2026-08-24/REPORT.md).
- A historical task-continuity candidate separated implicit turn completion
  from durable multi-turn task closure. At that experiment's surface revision,
  `task.complete` was catalog-cold during ordinary work and leased by explicit
  closure intent or a task-owned requirement; production v5 now always loads
  it as recorded below. `capability.manage` discovery remained available. Deterministic
  tests prove ordinary vs explicit surface selection, clean one-shot commit,
  and failed-sibling recovery. A live `add_test` smoke passed in 3 rounds / 2
  calls / 0 failures. Two initial long-flow A/C diagnostics then removed every
  incidental task closure and reduced C to 49/44 and 57/52 rounds/calls,
  versus A's 50/45 and 47/38. Median C model input stayed 24% below A,
  selected tokens 76% below, and final resident bytes 80% below. This is the
  first candidate to bring C execution close to A without expanding Context
  in those two pairs.
  It is **not accepted yet**: C hidden success was 3/4 then 4/4 while A was
  4/4 twice. The miss was a committed `RELEASE.md` update that wrote
  `Version 2` instead of the checker-required literal `v2`, not a missing edit
  or filesystem failure, but it still keeps the success-neutral gate open.
  Do not claim convergence or M15 closed. See the combined
  [`task-continuity report`](../crates/agent-eval/evidence/longflow-pinai-luna-task-continuity-2026-08-24/REPORT.md)
  and the
  [`independent repeat`](../crates/agent-eval/evidence/longflow-pinai-luna-task-continuity-r2-2026-08-24/REPORT.md).
  A later complete pair is a required counterexample: both arms passed 4/4,
  but C regressed to 82 rounds / 76 calls / 415,897 input tokens versus A's
  47 / 36 / 291,034, with a 30-round edit-repair turn. Task completions stayed
  zero, so the old task-affinity loop did not recur. The trace instead showed
  a committed patch that omitted a final module terminator, a prefix-only
  success echo that hid the file tail, and repeated ordinal repairs against
  ambiguous `}` anchors. `edit.patch` now requires a unique exact anchor on
  the model surface (legacy `occurrence` is parser-only) and bounded success
  echoes retain both head and tail. A post-change live `add_test` smoke passed
  in 4 rounds / 3 calls / 0 failures with the first patch committed and no
  confirm read or fallback. That proves interface compatibility, not a causal
  long-flow improvement; convergence remains open. See
  [`post-continuity r3`](../crates/agent-eval/evidence/longflow-post-continuity-r3-2026-08-24/REPORT.md).
  The first post-hardening unchanged-workload pair then passed 4/4 in both
  arms: C recovered to 53 rounds / 51 calls / max-turn 7 versus A's 54 / 44 /
  13, with zero C failed outputs. C input was 37% lower, historical Context
  66% lower, selected tokens 76% lower, and resident bytes 80% lower. No
  ordinal field or task completion appeared in C's trace. C still used seven
  more calls; three were reads of zero-byte successful verification artifacts.
  Process output now omits `artifact_ref` for zero-byte captures while keeping
  non-empty/truncated artifacts unchanged. Do not synthetically subtract those
  calls from the measured result. This is one positive dirty-tree pair, so the
  formal gate remains open pending an independent repeat. See
  [`unique-anchor r4`](../crates/agent-eval/evidence/longflow-post-edit-anchor-r4-2026-08-24/REPORT.md).
  That independent post-output pair also passed 4/4 and eliminated empty
  artifact reads, but did not close convergence: C was 47 rounds / 44 calls /
  max-turn 6 versus A's 43 / 32 / 6. C retained 21% lower model input, 57%
  lower historical Context, 69% lower selected tokens and 76% lower resident
  bytes, while using twelve more calls. The gap was evidence/discovery (29 C
  evidence-only results versus 16 A), concentrated in a targeted already-done
  turn where globally novel Git/catalog facts kept resetting the old global
  Evidence Frontier. Runtime now treats frontier progress as task-relevant
  after an exact Fresh directive target exists: unrooted novel evidence is
  retained but does not clear convergence debt; open-ended directives keep
  broad exploration. Exact current selected bodies co-locate a currentness
  marker, and verification recipe ids are explicitly values of `verify.run`,
  not tool names. Context/GC and autonomy are unchanged. This correction
  postdates the measurement, so a new pair is required. See
  [`task-relevant frontier r5`](../crates/agent-eval/evidence/longflow-post-empty-artifact-r5-2026-08-24/REPORT.md).
  That pair is now a counterexample rather than acceptance. Both arms stayed
  hidden 4/4 and C kept the Context advantage (input -21%, historical -54%,
  selected -65%, resident bytes -72%), but C rose to 57 rounds / 56 calls /
  max-turn 15 versus A's 49 / 38 / 7. The advisory fired but did not stop an
  already-satisfied turn from spending 15 rounds and 16 calls, including six
  repeated `git.status` calls. Exact surface events show the lower-level
  amplifier: loading `git.status` immediately displaced a just-loaded
  `git.diff`, so sequential inspect/load decisions could not assemble a
  cooperating tool set. Runtime now separates pending explicit loads from
  one-decision result delivery. An explicit load remains rooted until exact
  use, unload, or directive end; using one member consumes only that member,
  with no round TTL or Context change. Deterministic cohort and lifecycle
  tests are green, but this correction postdates r6 and still needs an
  unchanged-workload pair. See
  [`task-relevant frontier r6`](../crates/agent-eval/evidence/longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md).
  The r7 pair confirmed the cohort mechanism but rejected it as a sufficient
  convergence fix. Both arms remained hidden 4/4, the old Git churn fell from
  `git.status` 7→1, `git.diff` 2→1 and `capability.manage` 15→10, and max-turn
  recovered 15→8. C still used 62 rounds / 59 calls versus A's 46 / 35.
  Eight of C's ten catalog operations addressed universal coding primitives,
  while the compact `fs.write` + Git schemas cost only about 190 tokens per
  round. The next isolated candidate therefore moves `fs.write`, `git.status`
  and `git.diff` into the stable production core (about 947 total schema
  tokens, under the unchanged 4,096 cap); effect authority and Context are
  unchanged. This candidate postdates r7 and must be reverted unless an
  unchanged pair reduces rounds/calls at hidden parity. See
  [`pending tool-cohort r7`](../crates/agent-eval/evidence/longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md).
  Three unchanged stable-core pairs now support retaining that boundary. r8 was
  C/A 46/46 rounds and 41/37 calls; r9 was 49/47 and 46/38. All six arm-runs
  passed hidden 4/4, `capability.manage` was one call per arm in r8/r9, and C
  kept 22–39% lower input plus 61–67% lower historical Context. This reduces the
  pre-surface r7 gap from +16 rounds/+24 calls to 0/+4 and +2/+8. It does not
  close convergence: r9 C max-turn was 9 versus A's 7 and its Hello edit
  needed three patch calls after two sequential hunks targeted the same anchor.
  `edit.patch` now requires explicit model-visible `replace`, `insert_before`
  or `insert_after`; inserts preserve their unique anchor, while omitted op
  remains parser-only replace compatibility. The unchanged r10 live repeat
  passed hidden 4/4 in both arms at C/A 48/47 rounds, 41/39 calls, identical
  three failed outputs and max-turn 8. C used 39% less model input, 67% less
  historical Context, 78% fewer selected tokens and 84% fewer resident bytes.
  Explicit inserts were exercised; the r9 conflicting-anchor Hello tail did
  not recur. The remaining two C patch refusals were exact-match-safe
  `ambiguous_match` / `no_exact_match` locator errors and recovered without a
  filesystem settlement failure. Across r8-r10 the median gap is +1 round / +4
  calls. Retain the stable core and explicit operations; do not add positional
  or fuzzy edit authority from one ambiguous sample. See
  [`stable core r8`](../crates/agent-eval/evidence/longflow-stable-core-surface-r8-2026-08-24/REPORT.md)
  and
  [`stable core r9`](../crates/agent-eval/evidence/longflow-stable-core-surface-r9-2026-08-24/REPORT.md),
  then
  [`explicit edit operations r10`](../crates/agent-eval/evidence/longflow-explicit-edit-ops-r10-2026-08-24/REPORT.md).
- Evidence-backed coherence correction after the post-resolver longflow:
  TaskProgress identity no longer erases the only selected file body.
  Descriptor pricing requires exact same-request `path@revision` body
  presence; checkpoint restoration selects actual spill demand and spends
  the existing hash-only revalidation quota on those identities first.
  Unknown safety, context/send budgets, GC thresholds, and model autonomy
  are unchanged. Same-result currentness repair is now
  `EvidenceReconfirmed`, which does not clear convergence debt; its dormant
  fingerprint stays bounded and never enters TaskProgress.
  One production-surface live diagnostic passed both hidden arms and moved C
  from the preceding two-cell mean 65.5 rounds / 90 calls to 51 / 64 while
  used-round model input fell 24%; see
  [`longflow-body-coverage-2026-08-23/REPORT.md`](../crates/agent-eval/evidence/longflow-body-coverage-2026-08-23/REPORT.md).
  This is directional `n=1` evidence only: C still exceeded paired A, wall
  time did not improve, and C provider-total tokens are a retry-induced lower
  bound. Context itself retained its advantage: historical tokens -59%,
  selected tokens -71%, resident bytes -71%, and per-round model input -19%
  versus A. Extra rounds inflated TurnFrame/schema enough to leave whole-task
  used-round input +3%; optimize execution amplification without retuning
  Context. Do not claim the residual convergence problem closed.
- The 2026-08-24 execution-amplification audit kept one narrow protocol
  improvement and rejected one prompt-level shortcut. Bounded TurnFrame
  checkpoint receipts (at most six body-free outcome rows) activated in only
  one C round, so they are correctness/observability rather than a convergence
  claim. A cross-turn `TaskProgress.task_changes` projection was tested and
  fully reverted after its refinement amplified C to 127 rounds / 174 calls.
  On the retained-receipt run both hidden arms passed; C still used 52% less
  historical context, 66% fewer selected resident/reactivated tokens, 73%
  fewer resident bytes, and 21% less input per round than A, while extra rounds
  left whole-task input 2% higher. Context stays frozen; the next execution
  candidate must pass deterministic replay plus paired-live long-tail gates.
  A subsequent two-repeat generic "current workspace is authoritative"
  system-policy candidate also failed that gate (C 64/79 and 72/76 versus A
  44/30 and 43/29) and was reverted; it induced repeated completion and
  verification activity across unrelated turns. Event-only reaggregation now
  isolates the retained-run floor: C/A had the same eight Known mutation
  outcomes, while evidence-only results were 48/21, Unknown invalidations 9/0,
  and the maximum outcome-free result streak 18/3. C exposed 134 reported
  catalog-optional rows (118 unused in their round) versus A's 28 (26 unused).
  The complete +36 call gap partitions into +27 evidence-only and +9 Unknown
  results. `agent-eval` now renders/bundles these outcome-shadow and optional-
  surface metrics. A first runtime behavior slice now uses source-driven
  schema leases: exact called tools survive until their result is consumed;
  explicitly loaded but unused tools form a directive-local pending cohort
  until exact use, unload, or turn end. Host/operator loads are a separate
  persistent source until explicit unload; Runtime/model loads never become
  task-global pins, and checkpoints carry residency rather than minting host
  intent. Explicit task and typed need roots stay loaded; catalog reload
  remains available. A bounded
  `ExecutionBatchSettled` ledger now counts transient/refused/reused results
  without entering Context or the prompt. Oversized provider batches execute
  no member but still receive exact no-dispatch terminal accounting. Lease or
  batch audit-write failure now fences before another model decision.
  A second execution-only slice adds fail-closed pre-dispatch tool purpose and
  target attribution, an eight-row revision-bound negative-path fact table
  with live Workspace checks before no-dispatch reuse, typed lifecycle events
  and eval counters, plus exact trusted-verifier source affinity under the
  current task-anchor revision. Dynamic capability roles and output metadata
  cannot self-authorize verification; generic shell/process remain Opaque.
  A third slice adds host-opt-in `ExactCurrentWorld` PASS reuse on the existing
  bounded verification facts. The equivalence tuple is task state + anchor +
  user directive + workspace revision + exact tool/argument digest + a host
  recipe/profile/policy/environment identity digest; raw environment material
  is never stored, and any mismatch executes normally.
  Reuse requires a durable body-free lifecycle event, returns a truthful
  `executed=false` terminal result, and has separate eval counters. A new user
  directive always permits a real rerun. Production now exposes bounded
  `verify.run { recipe_id }` only when the composition root discovers recipes;
  model argv cannot replace host argv and unknown ids have no process
  authority. General project runners remain TaskScoped/Unknown-safe. The
  generic manifest-free Rust test-target compile is the first exact
  source-read-only recipe and binds a complete bounded workspace input
  snapshot, platform, compiler and environment. Transitive sibling modules are
  covered; links/escapes, external-input directives, special files, overflow
  and pre/post identity drift downgrade and execute normally.
  Deterministic contract/state/builtin/dynamic/actor tests pass (including two
  terminal missing-read results and two terminal verification results, each
  from one real dispatch). The fourth real-runtime Convergence Bench scenario
  also proves `verify.run` requested twice produces one spawn, two successful
  terminal results and Recorded/Reused = 1/1. A two-repeat paired live attempt
  on 2026-08-24 is deliberately excluded from performance and success-rate
  claims: all four arm-runs ended on the same retryable provider transport
  failure, with incomplete usage and lower-bound token accounting. The two
  arm-runs that reached `verify.run` both recorded a successful PASS; neither
  requested an equivalent second verification before transport failure, so the
  attempt cannot measure live reuse or C/A convergence. The live gate remains
  open; see
  [`longflow-exact-verification-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-exact-verification-2026-08-24/REPORT.md).
  One independent retry was also excluded: C lost its first provider request in
  both repeats while A failed later, proving severe asymmetric censoring rather
  than an execution comparison. Stop live reruns until the provider is stable;
  see
  [`longflow-exact-verification-rerun-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-exact-verification-rerun-2026-08-24/REPORT.md).
  Broader host-declared equivalence, in-flight joins and obligation-scoped
  provenance sources remain open behind ROADMAP gates.
  Context/GC selection
  is unchanged, so the measured C context advantage remains the baseline to
  preserve.
  Prior retained evidence:
  [`longflow-task-provenance-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-task-provenance-2026-08-24/REPORT.md).
- The first long-task Runtime slice landed (deterministic only): a
  catalog-cold `task.manage` progress proposal applies through the
  existing anchor compare-and-swap at operation-commit time and writes
  the authoritative outcome back into the model-visible result, so a
  stale base revision refuses without touching task state and is
  retryable in the next round. It can update only autonomous fields —
  current interpretation, plan progress, open loops and one replaceable
  `next_action` — so user goal/constraint authority stays structurally
  on the boundary/approval path. Success publishes `TaskAnchorChanged`
  followed by `TaskProgressUpdated` in event-provable order; refusals
  publish only the typed outcome. Deterministic coverage: tool bounds and
  deny-unknown-fields, accepted/stale actor paths, checkpoint round-trip,
  and single-render prompt projection. Conformance surface tables were
  aligned with intent-gated completion (`task.complete`/`task.manage`
  are catalog-cold; the unload path tests a genuinely optional tool).
  The later safe-point, completion and first live-pilot results are summarized
  below; this slice alone made no live claim.
- The second long-task Runtime slice landed (deterministic only): fully
  settled batches accrue bounded checkpoint debt (anchor change, durable
  workspace mutation, verification change); debt installs the bounded
  resume into the existing task record and schedules exactly one atomic
  write under the workspace state directory. `TaskResumeCommitted`
  precedes `CheckpointDurable`, which lands before `TurnCompleted`; a
  failed write re-arms the debt as `CheckpointWriteFailed`. Completion waits
  for in-flight writes. Continuation also waits for settlement but currently
  does not reject a failed-write outcome; the later watermark implementation
  landed, but the 2026-08-27 review reopened `LONGTASK-04` because those
  watermarks alias task-anchor revision rather than snapshot identity.
  `continue_active_task` starts a fresh turn from the stored current
  directive and resume state with a `task_continuation` input kind — no
  new user instruction, no re-ingest, `TaskContinuationStarted` is
  event-visible. Read-only rounds accrue nothing. Deterministic
  coverage: ordering, no-debt read-only rounds, store atomicity and
  fail-closed locations, and continuation identity. LT-RUN-03 and the first
  `retry_policy_dev` pilot have since run; their remaining proof gaps are
  summarized below and in
  [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).
- The third long-task Runtime slice landed (deterministic only): the
  completion acceptance gate re-runs at every edge — no recovery fence,
  no unsettled cancelled operation, zero open failure obligations,
  required verification current, and no open loops silently erased. A
  gated one-shot proposal returns its decision to the model with one
  warning per turn instead of committing; deferred and `/done`
  completions fail with the typed reason. The intended successful order is
  `TurnCompleted` -> final durable checkpoint acknowledgement ->
  `TaskCompleted`, and JSONL proves that order. The 2026-08-27 review found
  that the acknowledged final snapshot can contain inconsistent task authority
  and fail restore validation, so order alone is not completion-durability
  proof. The current failed-write warning occurs after task authority was
  cleared and cannot make it active again; `LT-RUN-05` replaces that behavior
  with two-phase completion that stays pending/retryable until durable ack.
  Deterministic coverage: open-loop refusal with later resolution and
  full ordering proof. All three deterministic LT-RUN slices are green; the
  first live `retry_policy_dev` normal/resume cells later ran as summarized
  below.
- The frozen `retry_policy_dev` fixture landed with its deterministic
  layer-1 gate green (2026-08-25, `agent-eval --long-task-gate` plus a
  cargo test running the same gate). One scripted normal/resume pair
  drives two real runtime instances over the production tool surface:
  phase one reads the fixture, records bounded progress through the
  anchor CAS and durably mutates `src/config.rs`; the harness stops the
  runtime, restores a fresh instance across runs through the shared
  durable authority lineage and continues the SAME directive via
  `continue_active_task`; phase two implements the retry policy,
  updates the README/error taxonomy and closes through intent-gated
  `task.complete`. Directive tools are catalog-cold optionals: the
  scripted model leases them via `capability.manage`, matching what any
  live run must do. Acceptance predicates: resume commits > 0, durable
  checkpoints >= 2, continuation and completion events present,
  byte-exact final workspace, empty hidden-check violations, and
  positional ordering `TurnCompleted` -> final durable ->
  `TaskCompleted`. This is a scripted deterministic result, not proof that the
  persisted safe-point artifact can cold-load all Runtime planes. Landing the
  gate exposed a real settlement-order
  defect and fixed it: an accepted `task.manage` advances the authority
  epoch during its own operation commit, so recording the accepted value
  completion afterwards saw a stale epoch and raised a false recovery
  fence that refused later mutations; accepted values now terminalize
  before directive application. The first live C cells (evaluation layer 2)
  are summarized next; layers 3+ remain open behind the `LT-RUN-05`
  correctness gate.
- The 2026-08-27 post-landing review supersedes the earlier `LT-RUN-04`
  completion wording. The four implementation slices and their deterministic
  suites are present, but Slice B, Slice D and the live evaluator are reopened
  for correctness:

    - the final completion checkpoint may be acknowledged after active-task
      authority has been cleared but before the Runtime task identity is
      cleared, producing a durable artifact that restore validation rejects;
    - safe-point required/durable watermarks alias task-anchor revision although
      workspace and verification state may change without advancing it, so an
      older checkpoint may satisfy a newer basis;
    - automatic capture does not use the external path's stable capability-
      generation handshake, and checkpoint read/retention is not yet bounded;
    - the live harness accepts any later `CheckpointDurable` instead of matching
      the latest committed snapshot by sequence, checksum, artifact and
      capability generation;
    - progress-only anchor CAS is interpreted differently by completion
      acceptance, exact verification reuse and `CompletionOpportunity`; and
    - oracle setup/start failures, runtime/provider failures and failed-resume
      accounting are not yet classified strongly enough for a decision-grade
      result.

  Landed since that review (2026-08-27), the first LT-RUN-05 work package:
  actor-owned monotonic snapshot sequences replace the anchor-aliased
  durability watermarks; acknowledgements retire exactly their artifact's
  frozen debt set; continuation requires no outstanding debt, no in-flight or
  failed write, and a landed sequence; the allocator watermark rides the
  checkpoint lineage without regressing on restore. Runtime suites and the
  scripted normal/resume gate are green with the fence active. Work package
  two landed on top: safe-point, instance and terminal capture share one
  generation-handshaked assembler that validates before persisting; terminal
  completion is two-phase (durable prospective-terminal ack before any
  in-memory commit or TaskCompleted; failed writes leave the task
  pending/retryable); and the store enforces header/payload/artifact byte
  caps plus bounded newest-window retention.

  Earlier raw artifacts remain retained as diagnostic evidence, but their
  pass/fail ratios and medians must not be used for promotion.
- The `CompletionOpportunity` off/on gate did run twice on 2026-08-25
  ([`evidence/opportunity-gate/REPORT.md`](../crates/agent-eval/evidence/opportunity-gate/REPORT.md));
  this replaces the stale statement that it had not run. Attempt 2 proves one
  live offer -> lease -> explicit `task.complete` -> committed closure chain,
  and the deterministic already-satisfied replay remains green (now also
  observing the offer's checkpoint-debt diagnostic). It does not
  prove product benefit or safe promotion: the candidate remains default-off.
  The 2026-08-27 prerequisites for another live window have since landed:
  EXEC-REV-01's independent verification basis, EVAL-05's durable-tuple
  resume correlation and EVAL-06's typed oracle setup classification are
  all fixed with deterministic coverage, and on 2026-08-28 the cold-resume
  matrix itself landed: the scripted gate's resume phase consumes only the
  acknowledged artifact tuple through the verified cold-load path, the
  terminal artifact restores into a third fresh instance with the completed
  task plane visible, capability generation rides capture and every
  acknowledgement, and retention gained an aggregate byte budget.
  `task.complete` joined the always-loaded production surface (surface rev
  v5) as a product choice; the completion acceptance gate remains the sole
  closure authority and refuses premature or unverified proposals. The
  three same-day M15 attempts do **not** validate that surface choice or a
  serving choice. Their v2 bundles projected missing closure as Runtime
  failure despite M15's report-only closure contract, stamped every pack
  with the retry-policy identity/digest, inferred provider health from error
  text, and were summarized by hand with inconsistent arithmetic. The relay
  attempt's six `max_output_tokens` results are model-output-limit failures,
  not proven transport outages. All three attempts are now forensic-only in
  `evidence/m15-window/REPORT.md`; their ratios and apparent deltas cannot be
  used for promotion or causality.

  The historical evaluator repair landed as `retry-pilot-cell-v3`: actual pack
  identity/digest, acceptance-profile-aware verdicts, typed Runtime/provider/
  model/harness failure classes, independently persisted restore/exact-tuple/
  continuation/turn/task facts, and an exact window manifest whose report is
  regenerated from the 12 immutable cell directories. Prospective evidence now
  uses `retry-pilot-cell-v4`, adding stable pair/source identity, independent
  acceptance-declaration revision/source identity and bounded request-audit
  facts. Its reporter requires all 16 identity/switch keys and recomputes the
  frozen identities. Provider transport or harness failure yields NOT_RUN and
  censors a window; `max_output_tokens` yields a model-output-limit cell FAIL.
  Formal execution rejects dirty source, pack/repeat drift and protocol `auto`.
  M15 remains open until the current exact candidate passes its deterministic,
  clean-source/CI, product-preflight and single-serving v4 window gates.

  The 2026-08-28 bounded representative preflight pinned PinAI `/v1`,
  `gpt-5.6-luna`, Responses protocol and a 128,000-token context window for its
  source-bound dirty-tree diagnostic cell
  (`retry_policy_dev`, normal, closure-required) passed behavior, diff and
  committed closure in 26 rounds / 59 tool calls / 3 failed outputs /
  315,468 ms, with zero provider retries and a contiguous observed event
  suffix. It is historical serving-selection evidence only, not a formal M15
  cell, a current-source pin or a failure-rate estimate. The next exact-source
  preflight must pin its own unchanged tuple. An earlier preflight on the same
  serving failed
  closure after 30 rounds / 53 calls / 7 failed outputs; comparing two
  stochastic cells cannot establish a causal round/call improvement.

  The passing cell also localizes cost away from Context selection:
  cumulative historical-context prompt cost was 8,146 tokens versus 119,912
  TurnFrame tokens (model input 324,783; output 10,531). One of the three
  failed outputs was `fs.write` refusing a missing `tests/` parent; recovery
  then consumed three model decisions to load `shell.exec`, create the
  directory and retry the write. Preserve `fs.write`'s existing-parent
  transaction boundary. `TOOL-DIR-01` has now landed deterministically as
  `fs.mkdir`: one exact final component, existing immediate parent, pinned
  handle, authority-v3 Prepared/committed object identity, exact-empty
  rollback and conservative reopen recovery. `fs.write` now names this typed
  recovery path. The tool stays catalog-cold; the `TOOL-DIR-SURFACE-01`
  deterministic admission gate landed (2026-08-28): a failing mutating
  result whose typed metadata names the first creatable directory surfaces
  exactly `fs.mkdir` with `RecoverySurface` provenance for one decision —
  exact-tool provenance, one-decision source lifetime, approval unchanged
  (PreferSurface demand only; a read-only gate still refuses the
  recovery-marked write without dispatch), and no surface change for
  unrelated missing reads. The candidate ships behind a host switch
  (default off). The full 24-cell isolated live paired run completed the same
  day (`crates/agent-eval/evidence/recovery-surface-gate/REPORT.md`), but a
  post-run audit found zero `RecoverySurface`/`next_directory` exposure in all
  24 event streams; all eight policy cells catalog-loaded and successfully
  called `fs.mkdir`. Its off/on differences therefore cannot be attributed to
  the candidate. Status is `NOT_EXERCISED / no promotion`: retain the
  catalog-cold baseline and keep the switch off conservatively, but do not
  advance the always-ready fallback or claim the candidate caused the 55-round
  tail. The diagnosis failure is also evaluator calibration: the checked-in
  golden solution fails its own saturation oracle and fixture self-check never
  runs that oracle. Calibrated 2026-08-29 (fixture authoring, frozen
  task/oracle meaning): the diag golden saturates via `u128` widening, the
  directive and `DIAGNOSIS` name the saturate-not-wrap edge, the hidden check
  demands an overflow-safe marker, and fixture self-check runs each M15 pack
  oracle offline against seed and scripted solution; diag digest regenerated
  to `2fff5157…eeb`, migrate digest unchanged. The evaluator-validity part of
  the pre-window checklist is done, and the one-cell product preflight on the
  observation-foundation source is cleared: `retry_diag_dev` normal
  PASSed 2026-08-29 on the same pinned serving at clean HEAD `09cce69`
  (with the same frozen diag digest) in 14 rounds / 22 tool calls /
  1 failed output / 139,886 ms — zero provider retries, contiguous events,
  6 durable checkpoints, hidden oracle green, and settlement exposed (`seen`,
  pre 9/15 → post 5/7) with ordinary-final closure, no `task.complete`, no
  auto-close. The only unmatched diagnosis marker is the `backoff.rs`
  overflow-safe needle: the written `exponent >= u64::BITS` + `checked_mul`
  + saturation shape beats the oracle but not the reference
  `u128`/`leading_zeros` needle text, a needle-shape miss, not a functional
  failure. The calibrated diag fixture is solvable on the pinned serving; the
  earlier 2-cell `--diag-smoke` failure was the model not solving the
  overflow edge, which the calibration's needle and oracle now reject
  consistently. The same one-cell preflight then passed the resume arm the
  same day at clean HEAD `65f6cc8`: two resumed turns (5 + 4 rounds) /
  19 tool calls / 0 failed outputs / 104,516 ms, hidden oracle green,
  settlement exposed (pre 8/19 → post 1/0) with ordinary-final closure, and
  the same single needle-shape marker miss. Both arms of the one-cell
  product preflight are therefore cleared on the frozen fixture. The first
  formal clean-tree v3 window ran 2026-08-29 on that pinned serving at clean
  HEAD `16ba7c4` (protocol pinned `responses`, 12 cells, 0 NOT_RUN): 11/12
  PASS; the single failure is `retry_diag_dev` resume r2 (six+five resumed
  rounds / 25 tools / 1 failed output, hidden oracle not satisfied on the
  overflow edge — the same `backoff.rs` needle miss plus one failed
  `edit.patch`). Every other cell passes behavior, diff and, where resumed,
  exact-tuple restored-and-continued; closures are 8/12 `task.complete` and
  4/12 ordinary-final, reported not gated. Efficiency facts (mechanical
  report,
  [`evidence/m15-window/_windows/1787966622822/REPORT.md`](../crates/agent-eval/evidence/m15-window/_windows/1787966622822/REPORT.md)):
  rounds total/max 137/21, tools total/max 332/49, wall max 712,990 ms,
  provider input/output tokens 1,408,538/59,419 (lower bounds where a resume
  cell's usage is incomplete). The diag overflow edge is the one recurring
  failure surface across preflight and the window, consistent with its
  calibrated difficulty. M15 remains open: the frozen §4 verdict passes the
  development plane only when all 12 cells pass, so this window is a valid
  failed result. A second clean-tree v3
  window ran the same day at clean HEAD `f625d39` (protocol pinned
  `responses`; cached input now metered, `cached 152,576 / input 1,943,439`
  across the window): 9/12 PASS — all three failures are diag cells
  (normal r1, normal r2, resume r1; resume r2 PASS), while
  `retry_migrate_dev` and `retry_policy_dev` pass all 8 cells with
  exact-tuple restored-and-continued everywhere and 0 NOT_RUN. Across both
  windows, the diag overflow edge is the only recurring failure surface
  (first window 3/4 diag PASS, second 1/4), consistent with its calibrated
  difficulty and with a stochastic per-cell solve rate; the fixture,
  oracle and serving stayed unchanged. A third clean-tree v3 window ran the
  same day at clean
  HEAD `779604559f682dddc54018e99e5fb35b0080e965` (same pinned tuple;
  cached input 200,704 / input 1,942,278): 10/12 PASS — the two failures are
  again diag cells (normal r2, resume r1), and `retry_migrate_dev` +
  `retry_policy_dev` have now
  passed 24/24 cells across all three windows with exact-tuple
  continuation everywhere; 0 NOT_RUN. Diag-cause analysis over the three
  formal windows is fully attributed: every failing diag cell (6/6)
  finalizes the fix with
  `checked_shl(exp).unwrap_or(u64::MAX)` — which only guards shift counts
  ≥ 64, not bits shifted out (`100u64.checked_shl(62)` is `Some(0)`) —
  while every formal-window passing diag cell (6/6) uses
  `checked_mul`/`saturating_mul` (or
  `min(63)` + `checked_mul`); failing cells also self-test with `base = 1`
  configurations that avoid the trap, so their own tests pass while the
  oracle fails. The calibration makes the needle and oracle agree against
  the trap. Formal diag is 6/12 PASS (50%); all same-fixture calibrated
  `diag-smoke` plus formal evidence is 9/17 PASS (52.9%). The recurrence is
  not a harness or transport defect: it is a model/solver limitation on the
  pinned serving. M15 remains open with the diag overflow edge the sole
  recurring failure surface. Three valid failed windows are already evidence;
  a fourth unchanged retry is prohibited until the cross-window decision rule
  in `M15_ACCEPTANCE.md` is frozen prospectively.
- **Completion Convergence observation foundation landed; task-aware control
  plane remains open** (2026-08-29, CONV-CLOSE-01 reviewed): evaluator
  cleanliness now aligns model-visible
  workspace with the allowed-diff policy (`.gate/`, `target/` and
  `Cargo.lock` are gitignored by fixture self-check, so build artifacts
  cannot manufacture cleanup loops the evaluator silently discards);
  event-derived metrics aggregate the first execution-local settlement label;
  dynamic states `Working -> VerificationDue -> VerifiedCurrent ->
  SettledCandidate` derive from the bounded `TaskRecord.resume: ExecutionState`
  and publish label-on-change `ExecutionFrontier` events. Seven deterministic
  actor scenarios are green: ordinary
  final, durable closure, genuine remaining work, mutation after
  verification, stale verification, proposal settlement across
  cancel/resume, and cold restore. The stale runtime comment describing
  `task.complete` as catalog-cold was removed (the v5 registry always loads
  it). Post-review, this is observation evidence only.
  `TaskProgressView.settlement` is populated but
  `PromptAssembler::render_task_progress` does not render it, so the model
  never saw the claimed settlement one-liner. Eligibility also consults only
  verification validity and the execution-obligation ledger; it does not bind
  current user/task authority, acceptance coverage,
  `TaskAnchor.open_loops`/`next_action`, or `failed_commands`. Therefore the
  current `SettledCandidate` means only “execution state currently verified,”
  not “whole task ready to finish.” The live runner (`--conv-gate`,
  [`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md))
  ran 4/4 normal/resume cells PASS with 4/4 event exposure, but it has no
  projection-off control arm; normal versus resume is not an off/on pair.
  Model-chosen `task.complete` therefore cannot be attributed to settlement.
  `--conv-tail` also counts every event after the first candidate even after a
  later mutation reopens work, so it is not a causal efficiency metric.
  CONV-CLOSE-02 must correct task-aware eligibility, wire the projection only
  behind a default-off switch, replace lifetime tails with settlement episodes,
  and then run a real switch-off/on paired gate. Do not claim convergence or
  M15 closed from the current report.
- CONV-CLOSE-02 landed its four delivery steps the same day and ran the real
  switch-off/on paired gate (approved 8-cell budget, `--allow-dirty`):
  task-aware settle (fail-closed at `VerifiedCurrent` without declared
  acceptance coverage), the neutral fact behind the default-off
  `project_progress` switch with request-level tests, settlement-episode
  counters, and `evaluate_conv_gate` per-pair parity. Live cells required
  three bounded repairs first: trusted PASS clears identity-exact
  `failed_commands` on the current basis/directive/workspace tuple,
  request-level plus whole-cell provider retry, and live acceptance
  declaration bound by the trusted verification pass at observation time.
  The gate ran 8/8 cells PASS with 0 NOT_RUN but FAILED promotion: pair-0
  (normal r1) exposure asymmetry (off none / on seen; the off cell recorded
  no trusted verification pass because its model used only the TaskScoped
  `rust.workspace` runner — no exact identity, so the join never armed and
  the cell is inconclusive by rule, while exposed cells used the host
  `jobrunner.exact` recipe whose pass arms synchronously), marker-violation
  parity in 3/4 pairs (needle-shape misses the behavioral oracle
  tolerates), and episode-rounds/calls medians 1→1 (not strictly lower).
  Projection rendering was real and arm-separated: off 0 tokens every
  round, on 430–512 tokens once a candidate existed. Per the frozen rule
  the projection stays default-off and the gate returns to observation; do
  not claim convergence or M15 closed. See
  [`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md).

  The
  frozen CompletionOpportunity off/on paired live gate then ran
  decision-grade on 2026-08-28 (8 cells): it FAILED promotion — the off
  baseline closed a normal cell by itself while no on-cell improved closure
  (one offer armed, its lease was not called) — so per the frozen rule the
  candidate ENDS default-off. `retry_policy_dev` behavior and diff
  dimensions passed in all eight cells; the truth chain held under live
  load throughout.
- **Execution Convergence V1 mechanism landed** (2026-08-23, all 22
  items checked — the checklist is now the historical record
  [`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md)):
  Evidence Frontier + FrontierDelta + `ExecutionFrontier` events and
  eval metrics; RetryDomain (`ExecutableResolution`, no K-strikes);
  per-turn protocol body cache with checkpoint-gated rehydration and
  event-level hit/miss accounting (`ProtocolBodyCacheStats`);
  versioned `HostPolicySnapshot`; unified surface pressure budget;
  replay frontier rebuild + conformance serde contracts. Verification:
  `agent-eval --convergence-bench` four deterministic scenarios PASS
  on the real runtime + real tool surface.
- Clean A/C longflow runs completed 2026-08-23. The post-obligation
  run (n=2, all four arm-runs passed hidden verification): C r1 61
  rounds / C r2 64 rounds, A r1 61 rounds / A r2 47 rounds — first run
  with live cache accounting, which showed hit rate 0 under command
  pressure (every Unknown-footprint command cleared the whole turn
  cache) and guessing chains whose attempts never escalated because any
  same-domain success cleared the obligation. Both findings are fixed:
  the body cache now suspends on Unknown mutations and revives entries
  after BeforeModel revalidation proves the identical digest
  (PROTO-EVID-03); obligations are lineages with precondition epochs and
  fingerprint-matched resolution (CONV-03), event-visible end to end
  (CONV-OBS-01). Facts in
  [`crates/agent-eval/evidence/longflow-post-obligation-2026-08-23/REPORT.md`](../crates/agent-eval/evidence/longflow-post-obligation-2026-08-23/REPORT.md).
  Context GC and compaction policy stay frozen — C carried ~4.7 KB peak
  resident vs A's 231K historical-context tokens at equal rounds; do not
  reopen either from these numbers.
- Trust & Obligation first cut landed (22-item program complete,
  historical record
  [`TRUST_AND_OBLIGATION_TODO.md`](TRUST_AND_OBLIGATION_TODO.md)):
  Evidence Frontier + FrontierDelta + `ExecutionFrontier` events;
  RetryDomain (`ExecutableResolution`, no K-strikes); per-turn protocol
  body cache with checkpoint-gated rehydration and `ProtocolBodyCacheStats`
  accounting; capability-output metadata sanitizing; real
  `ArgumentDigest` evidence identity; versioned `HostPolicySnapshot`;
  unified surface pressure budget; replay frontier rebuild +
  conformance serde contracts. Verification:
  `agent-eval --convergence-bench` four deterministic scenarios PASS
  on the real runtime + real tool surface.
- M12 first cut: structured `EffectIntent` + trusted `HostToolPolicy`,
  multi-file `WorkspaceWriteSet` bounds, and commit-time
  Actual ⊆ Approved (`MOD-AUTH-01`/`02`).
- M13 first cut: `SandboxProfile` vs post-spawn `SandboxCapabilities`.
- PLAT-06 slices 1–2 (lifecycle / cancel-ACK) landed. Multiplexing is
  not in v0.
- Scheduling/reliability fixes landed 2026-08-23 (AUDIT_TODO
  SCHED-01–04): idle-round `BeforeModel` maintenance gate; explicit
  search candidate completeness with bounded residual verification;
  same-class-across-targets `EXECUTION STALL` cluster escalation; and
  the `protocol-checkpoint-body-missing` reread motive instrument.
  The body cache itself is implemented (see above) with counters, so
  cache claims are verifiable from the event stream.
- CORE-11 registry layering landed 2026-08-23: builtin host policies
  moved out of contracts into `tool-runtime`; `agent-compose` owns the
  `HostToolPolicyRegistry` (builtins + fail-closed plugin `admit()`),
  wired into the kernel lease path, approval gate and dispatcher. The
  manifest → operator-review → atomic `admit_reviewed`/revocation flow and
  per-binding epoch fence landed by 2026-08-26. M12 remains open only for the
  bounded production-path closure audit below.
- Production always-load (surface rev v5, 2026-08-28): `fs.list`, `fs.read`,
  `fs.write`, `search.grep`, `artifact.read`, `edit.patch`, `git.status`,
  `git.diff`, `task.complete`, `capability.manage`. `task.complete` closure
  execution stays intent-gated by the completion acceptance gate. Their compact core schemas cost roughly 1k tokens total,
  still below the 4,096-token surface cap. Shell / `edit.replace` /
  `context.manage` and plugin tools are catalog-only; NeedEvidence
  PreferSurfaces `context.manage`.
- Scripted `--compare-arm` still additionally pins `edit.replace` /
  `context.manage`. Do not change that pin.
- Longflow parallel A/C is a separate product diagnostic and now uses the
  production-default tool surface; pair/cell evidence stamps
  `tool_surface=production`. It must not be used to silently change the
  frozen Context Mechanism pin.

**Historical status recorded 2026-08-27:** M12 and M13 were marked closed at
their named clean-tree gates (`platform-closure/m12/` and `/m13/` evidence
reports). Current authority wording is pending `GOV-STATUS-01`. **Do not claim
PLAT-06 closed**: slice 1–2 are landed and multiplexing stays out of v0.

- The typed host-trusted execution-facts channel reached its last behavioral
  consumers on 2026-08-26: context heating and observation identity now read
  `ContextIngress::ToolObservation.facts` (facts-first with per-value legacy
  fallback), the no-attribution verification entry reads its claim from
  dispatcher-lane facts under the same fallback rule, and the attributed
  production path keeps pre-dispatch attribution as the sole reusable-verifier
  authority. Values are identical for every producer class until trusted
  handlers stop stamping metadata keys, so no behavior change is claimed.
  Host-declared verification equivalence classes landed their first slice
  on 2026-08-26 and stay dormant until a host declares coverage domains
  through the recipe table (see
  [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)).
  Obligation-scoped provenance sources also landed 2026-08-26:
  `ExecutionObligation.source_tool_name` is stamped once by
  `record_obligation` from pre-dispatch truth, lease membership derives
  from live ledger rows, and `tool_lease_roots` folds it into runtime
  roots filtered against the catalog. Trusted handlers now stamp native
  typed execution facts at construction time under the reserved
  `_execution_facts` metadata key (sanitizer-stripped from untrusted
  producers), and every builtin family stamps an explicit
  workspace-mutation bound mirroring the temporary name table
  (`process.session` deliberately stays on the fallback pending its bound
  decision); per-handler tests lock native equals derivation. No
  model-visible output shape changed; a live fixture confirmed end-to-end
  behavior. The repository still contains legacy tracking ids in code comments;
  removing them is a bounded hygiene follow-up required by `AGENTS.md`, not an
  execution-semantics change. New comments keep tracking vocabulary in docs.

## Frozen

- GC knobs: `active_threshold` / `archive_threshold` / `gc_max_generation`
  (pinned by `gc_thresholds_are_freeze_pinned`).
- Frozen Context Bench SPEC / pack digest. No 27-cell or 300×3 rerun as
  v0 engineering.
- No embeddings, vector DB, RAG, learned router, Typed EpisodeOutcome,
  or new GC generation algorithm.
- ObservationMemo stays unwired.
- No `MOD-18`. Residual OS isolation is fail-closed for untrusted code,
  not a new slice. WASI is V2.
- Natural-language verify remains the four-needle hint.

## P0 / P1 — historical landed chronology

The current pre-M15 queue is the merged audit linked in **Now**. This section is
retained as dated implementation/evidence chronology; its older milestone
labels do not override `GOV-STATUS-01`, and non-conflicting residual backlog
remains in `AUDIT_TODO.md`.

**P0 — trusted execution closure-audit evidence (banked 2026-08-27).** The
gate history below is retained as context. Landed by 2026-08-26: the full
admission flow —
an installed package manifest supplies candidate tool names only, the
operator review artifact supplies the actual bindings,
`admit_reviewed` installs them atomically, and versioned snapshots bind
operation authority so an operator update never re-interprets an
in-flight operation — plus per-binding revocation fencing: a lease
stamps its binding's epoch at mint and commit refuses when that binding
was explicitly revoked or replaced since; other tools' in-flight
operations are unaffected, and snapshot identity never fences.
Adding `plugin.foo` policy does not stale an already-approved `fs.write`;
only replacing or revoking the same binding affects that tool's later
authority. Global "revision changed → all old leases invalid" is rejected
by design, and two concerns stay separate: the policy snapshot identity
prevents *reinterpretation* of approved operations, while the explicit
binding revocation epoch is the only mechanism that may fence live
leases — one revision field must not carry both meanings.
M12 is now a closure audit, not an unbounded implementation queue. Nothing
structural is left on the reserve/dispatch/ack path. The out-of-process
coordinator transport landed 2026-08-26 as a process-separated durable ledger:
`broker_host` opens the same `ReservationJournal`, while
`ProcessEffectBroker` journals each phase across the pipe and applies effect
bodies locally at the requester. Close M12 only when one bounded evidence table
shows every brokerable production effect crosses that path, crash windows
reconcile honestly, and authority/revocation fencing holds; generic
shell/process remain named non-transactional exceptions. Broker-owned remote
execution and HTTP/gRPC shells are not V1 requirements without a remotable
consumer.
Do not build a second registry. Attestation is actual enforced
capabilities; generic process tools stay non-transactional.
The bounded closure-audit evidence generator landed 2026-08-27: the
deterministic `agent-eval --platform-closure-m12` run (also a cargo test)
wrote its first PASS report — 28 resolved rows, zero unresolved — under
`crates/agent-eval/evidence/platform-closure/m12/`, covering every brokerable
family on the journaled reserve/dispatch/ack path, NotApplied/Applied/Ambiguous
crash reconciliation through journal reopen, per-binding epoch fencing,
generic-process exceptions executing against an empty journal, and two
independent out-of-process coordinator sessions sharing one durable ledger.
The M13 counterpart landed the same day: `agent-eval --platform-closure-m13`
(real child spawns, per-profile `required ⊆ actual` activation, both refusal
cases, mechanism-proof attestations) wrote its first PASS report — 8 rows,
zero unresolved — under `crates/agent-eval/evidence/platform-closure/m13/`.
  Both gates were then recorded as closed 2026-08-27 on clean-tree regeneration
  of the two reports (commit-bound source digests in each manifest); current
  closure wording remains governed by `GOV-STATUS-01`.
M13 is likewise a closure audit: structured attestation must validate enforced
evidence, activation must enforce `required ⊆ actual`, and unsupported native
`UntrustedGenerated` must fail closed. Universal native availability belongs
to the WASI/V2 candidate, not the V1 gate. Multi-file
`EffectIntent` and commit-time Actual ⊆ Approved (`MOD-AUTH-01`/`02`)
landed 2026-08-21 — do not reopen them without new authority evidence.
Tool Surface utility scoring stays out of scope until
obligation-scoped convergence has stabilized; do not couple the two
variables again.

**P1 — Context live evidence, not Context retune.** `context-mech.v2`
12-cell A/C live ran 2026-08-21; facts in
[`crates/agent-eval/evidence/context-mech/REPORT.md`](../crates/agent-eval/evidence/context-mech/REPORT.md).
Do not retune GC from it. `add_test` is Tool Surface
(`historical_context=0`), not Context. Engine packs foreground first
(actual tokens). GC-induced reread is `Warm` + `Stored` only.

**P1 — open-turn convergence evidence.** Execution Convergence first
cut landed 2026-08-21: MOD-OBS-01 (a refused mutation is still an
observation), MOD-PROG-01 (stall advisory + deterministic duplicate
refusal), turn checkpointing (`TURN_FRAME_KEEP_EXCHANGES`). The
late-semantic op5 reproduction ran 2026-08-21 (4 live A/C cells under
`crates/agent-eval/evidence/context-mech-convergence/REPORT.md`): the
48-round loop did not recur (r2 C passed in 29 rounds) and the new
machinery fired zero times. Loop persistence is stochastic, but a
2026-08-22 replay proved the edit failure environment was not clean:
all 11 multi-line `no_exact_match` refusals in those four cells were
the deterministic LF-view/CRLF-raw mismatch. Remaining convergence
work is still a deterministic harness where the loop actually forms,
not more Context live cells. Abrupt-loss replay evidence landed
2026-08-26: the agent-replay recovery report now flags tool batches
killed between dispatch and durable settlement with exact per-call
counts, and keeps settle-time missing/unexpected terminals as a live
integrity signal.

**P1 — Tool Surface edit reliability.** `edit.patch` stays the only
production-always-loaded mutation primitive; matching remains exact and no
parallel edit schema was added. The 2026-08-22 implementation now provides:

- LF/CRLF newline-token equivalence with physical-EOL preservation, literal
  lone CR/non-EOL bytes, bounded scans, and a 4 MiB result ceiling;
- one model-visible, revision-required `files[]` schema (the legacy
  single-file form remains parser-only compatibility), a JSON-quoted
  `fs.read` header carrying raw-byte revision/EOL facts, and a complete
  in-order revision manifest outside the bounded edit echo;
- sorted canonical path leases, one pinned bounded snapshot, duplicate-alias
  rejection, short exclusively-created staging names, staged handle/length/
  SHA verification (plus Unix name/inode binding or Windows deny-sharing),
  compare-before-replace, installed-byte verification before and after the
  authority acknowledgement, and preservation of Unix mode bits or the
  Windows readonly bit;
- for Core-managed writes, a synced authority-journal v2 intent before temp
  creation, carrying bounded byte lengths and SHA-256 before/after revisions;
  bounded reopen reconciliation removes only a confined, regular staged file
  whose name identity and complete content are reverified; file writes require
  an existing parent and never create unjournaled directory topology; and
- typed stale/exact-match refusals, bounded topology/candidate output, one
  1200-character multi-file echo that preserves both ends when the middle is
  omitted, explicit model-visible replace/insert intent with unique exact
  anchors (legacy omitted op and ordinal occurrence are parser-only), and
  honest `NotApplied` / `Applied` / `Unknown` settlement.

Unit tests and clippy are green. A post-unique-anchor `add_test` smoke passed in
4 rounds / 3 calls / 0 failures, with the first patch committed and no confirm
read or fallback; it is compatibility evidence, not a long-tail estimate. The
versioned `agent-eval.tool-edit.v2` pack
plus `agent-eval.tool-surface-edit.v3` gate also produced a source-bound r4
dirty-tree diagnostic pass over the current hardened implementation: 4
fixtures × 3 repeats, 12/12 raw-byte truth,
12/12 flow gate, 9/9 non-conflict first patch, 3/3 proactive stale routes,
zero patch refusal/fallback/confirm-read/recovery/unknown, and 42 rounds. Its
wall total was 164,417 ms and reported provider tokens were 258,325; it
preserved all r3 call-quality results while observing lower wall p50/p95. See
the r4 evidence `REPORT.md`. This proves the combined contract on that frozen
surface, not a general task-failure rate or a causal performance gain.

`TOOL-EDIT-02` is no longer waiting for another unchanged provider/model
window. On the v4 surface, both archived clean-tree windows reached strict
12/12 with zero confirmation reads; each scored gate 11/12 and
non-conflict-first 8/9. A third console-only window reached the full bar but is
not archival evidence. The only repeated archived miss is a byte-perfect,
revision-correct two-file patch whose hunk partition differs from the hidden
`exact_hunks` decomposition. These finite results support the frozen surface;
they do not prove a general editor-engine failure rate.

The product route is byte/revision/settlement truth: hunk partition is not
model-visible authority and no downstream consumer currently requires a golden
decomposition. `exact_hunks` is now versioned to accept byte-equivalent
decompositions while preserving submitted paths, strict final bytes, revision
discipline, atomic settlement and no-fallback/no-confirm-read checks; the gate
is `agent-eval.tool-surface-edit.v4`. Reverse that choice only if a real
consumer first documents a canonical-granularity requirement. One archival 4x3
confirmation window on the versioned gate is now landed:
`tool-surface-edit-v4-clean-tree-2026-08-26-r4` scored `strict 12/12 gate
12/12 non_conflict_first 9/9` with zero confirmation reads; do not spend more
live windows on the unchanged ambiguous contract.
Deterministic external-race, crash, journal-fault and — since 2026-08-26 —
disk-full coverage are landed: the feature-gated `test-faults` storage seam
injects storage-full refusals at the authority intent, the staged temp bytes
and the committed record, with fixtures pinning nothing-staged, rolled-back
cleanup of a truncated stage, and `Applied { complete: false }` recovery by
hash evidence. Broader staged-byte accounting breadth remains reliability
work; it is not evidence that
the 12-cell diagnostic failed. Clones share the lease and a second official
`Workspace::open` on the same root is refused by the authority-journal lock;
direct or authority-bypassing filesystem writers remain outside it, and
hash→replace is not a filesystem CAS. Typed rollback now confirms cleanup and
terminal journals or returns `RecoveryRequired`; staged/composite rollback
attempts every child with bounded diagnostics, and Core fences later mutation
instead of reporting a plain rejection. Runtime projects preparation-time and
commit-rejection cleanup uncertainty separately as
`execution_cleanup_recovery_required` and
`not_applied_cleanup_recovery_required`, without preserving proposed
revisions as facts. Core-managed prepare crash seams after authority intent,
stage sync, and review record are mapped and recover conservatively. The
trusted context-free prepare entry remains non-crash-recoverable; partial,
substituted, or colliding stage content is retained as `Ambiguous` rather than
deleted. A multi-file effect remains sequential with honest partial recovery.
This V1-candidate gate runs in parallel with — and does not replace or close —
the M12 → M13 mainline.

**P1 — long-task recovery integrity before further live promotion.** The
r8-r10 stable-core/edit sequence remains frozen: C's median gap was +1 model
round / +4 tool calls while retaining the large Context advantage. The current
audit does not weaken that Context finding; it invalidates later recovery and
evaluation claims. Keep Context selection, GC, prompt packing and the stable
tool surface fixed.

`LT-RUN-05` in [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) repaired
and re-proved the existing `LT-RUN-04` substrate rather than adding a new
planning algorithm:

1. introduce an actor-owned monotonic snapshot sequence independent of
   `TaskAnchor`; continuation requires no debt, no failed or in-flight write,
   and `durable_sequence >= required_sequence`;
2. make completion two-phase: validate and durably acknowledge a prospective
   internally valid terminal snapshot before committing in-memory completion;
   share the stable capability-generation capture path;
3. bind cancellation, restore and continuation to the exact durable-lineage/
   task/sequence/artifact/checksum/capability-generation tuple;
4. define one verification basis and currentness predicate shared by
   completion, exact reuse and `CompletionOpportunity`;
5. classify oracle setup, provider, Runtime, behavior, diff, closure, restore
   and continuation independently; require every mandatory dimension and no
   runtime error for PASS, and preserve failed-path round/call accounting; and
6. prove same-anchor multi-snapshot order, out-of-order ack, failed-write retry,
   final-artifact restore, stale capability generation and progress-only
   verification movement with deterministic tests.

The deterministic snapshot/cold-restore chain and evaluator reconstruction are
green. The retained-C CompletionOpportunity off/on gate then ran eight cells
and failed promotion, so that candidate has ended default-off; do not spend
another pair on it. The newer 55-round / 129-call tail established Completion
Convergence as the pre-M15 readiness task. `task.complete` was always visible
and its 18 calls in the 24-cell run all returned successful tool results; the
tail made no completion call. The first implementation landed useful
observation labels, events and tests, but review found that the label is not
rendered to the model, its eligibility is execution-local rather than
task-aware, its tail metric does not stop when work reopens, and its live runner
  has no off/on treatment arm. That historical judgment assigned the work to
  CONV-CLOSE-02; the 2026-08-30 merged audit supersedes it. Do not auto-close,
  resurrect CompletionOpportunity, add fixed stopping counts, or change
  Context/GC. Same-model A/C and broader diagnosis/multi-file twins remain after
  formal M15. Full CPL and model-visible TaskGraph research stay deferred;
  bounded criterion receipts are current work under `ACCEPT-RECEIPT-01`.

## Next milestone

The current directive is concise:

1. finish and record the semantic completion-liveness deterministic
   regressions without weakening any completion/effect/recovery gate;
2. run the complete local gate, reconcile applicable P1 exits or exact
   selected-path exclusions, and record Windows/Linux CI on one clean source;
3. when a supported serving is available, run one fresh exact-source preflight
   and at most one freshly predeclared 12-cell window;
4. close M15 only on a valid 12/12 formal result; then execute the Reliable
   Local Agent alpha and V1 route in
   [`ROADMAP.md`](ROADMAP.md#route-to-a-usable-local-agent).

The candidate chronology below is retained for audit context. It does not
override the four steps above.

The next milestone is a **materially new, mechanically convergent execution
candidate**, not another prompt tweak or an immediate rerun. The repaired-source
+ PinAI/Luna candidate was rejected by the valid 6/12 window at
`_windows/1788385151733`, and the attempt-incident admission candidate was then
rejected by the valid 10/12 window at `_windows/1788402676712`; the current
order is:

1. preserve both windows and their predecessors unchanged; do not rerun the
   rejected source/serving candidates;
2. diagnose the immutable cell streams: the diag overflow-edge misses, the
   policy completion-gate tails and the bounded-framer malformed-event are
   distinct observations until evidence proves a shared cause — **done
   2026-09-03**,
   [`evidence/m15-diagnosis-repaired-source/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-repaired-source/REPORT.md)
   confirms three distinct model-quality surfaces with no shared cause and
   no harness/transport/oracle defect;
3. select one bounded candidate from those facts. Do not retune Context/GC,
   weaken protocol bounds, add a fixed round stop or introduce TaskGraph —
   **selected 2026-09-03 by operator direction** ("fix the documented
   problems"): the workload-split slice "general `AttemptIncident` versus
   completion-debt admission", narrowed to its diagnosis-supported core and
   landed on `e897c5c`. A typed refusal that only judges instruction
   quality (`NoExactMatch` / `AmbiguousMatch` / `HiddenPath`; `NoSearchMatch`
   is read-only by construction) proves the trusted handler left the target
   resource untouched, so it stays visible in the result and the bounded
   negative-fact table but never opens a failed-command row. Refusals that
   signal a real resource reconciliation (moved revision → the
   refusal-lineage machinery; missing path → the debt row plus the typed
   directory-recovery surface) and unknown-effect failures (process exit,
   I/O) keep their debt semantics unchanged, so no debt is auto-cleared and
   the 2026-08-31 P1 guarantees keep their original tests;
4. pass the candidate's deterministic failure matrix, the applicable open P1
   exits (or exact out-of-path proof), and a newly recorded clean local /
   dual-platform CI source — the deterministic matrix is green (admission,
   negative controls, lineage/retention guarantees re-exercised on
   debt-opening rows, checkpoint round trip, contract enum coverage; full
   all-target workspace suite and strict Clippy pass), and `EVAL-PREFLIGHT-01`
   closed the same day with `agent-eval --doctor` green end-to-end; the
   recorded clean source is `03bc6d5` with dual-platform CI green
   (run `33703472111`, 2026-09-03);
5. run the same-checkpoint causal fork only if settlement projection changes,
   then one fresh exact-source product preflight and at most one freshly
   predeclared M15 window — the candidate does not touch the settlement
   projection (skip the fork); the fresh preflight **PASSED** 2026-09-03 on
   the clean source `51559d4` (attempt `r1-attempt6` under
   `evidence/m15-preflight/`, source tree digest `7afea564...`, serving
   recorded in the attempt manifest): `retry_policy_dev` normal, product
   surface, behavior/diff pass, closure completed, provider healthy,
   27 model rounds, hidden oracle green.
- The 12-cell v4 window on the attempt-incident candidate + PinAI tuple was
  **predeclared 2026-09-03 before its run** (M15_ACCEPTANCE §7 item 8):
  3 fixtures × normal/resume × 2 repeats, the product surface, the pinned
  serving tuple (`https://api.pinaic.com/v1`, `gpt-5.6-luna`, Responses,
  128,000-token context, 4,096 max output tokens), one uninterrupted
  `agent-eval --m15-window` run; the exact clean source identity was recorded
  at launch (cell-recorded source tree digest `0cecc539...`), no source
  change happened during the run, and the mechanically regenerated report is
  the only accepted verdict.
- The 12-cell v4 window ran 2026-09-03 on the predeclared clean source
  `38d458e` and is a **valid FAIL: 10/12 pass, 0 NOT_RUN** — the mechanical
  report at `crates/agent-eval/evidence/m15-window/_windows/1788402676712/`.
  Migrate 4/4 (7–19 rounds, clean continuation where resumed); diag 3/4;
  policy 3/4. Behavior and allowed-diff pass 12/12; provider healthy in
  every cell; closures 3/12 (all three policy cells that closed used
  `task.complete`). The two failures are `retry_diag_dev normal r2`
  (48-round phase-one budget; `task.complete` refused 18/18 over
  `acceptance_undeclared` + `operator_closure_only` + later next-action) and
  `retry_policy_dev resume r1` (48-round phase-two budget; `task.complete`
  refused 12/12 over verification-currentness/acceptance-coverage/open-loop
  blockers). Both failing cells carry functionally-correct workspaces whose
  injected oracle tests pass — the `checked_shl` diag trap did not recur
  (the failing diag cell wrote a correct `checked_mul`/`min(63)` shape and
  missed only the static `u128`/`leading_zeros` marker), and the 2026-08-31
  P1 admission guarantee held (the policy refusals never cite failed-command
  debt). Per M15_ACCEPTANCE §5 the valid FAIL rejects the candidate and
  returns to diagnosis; the window is not rerun. Post-window diagnosis at
  [`crates/agent-eval/evidence/m15-diagnosis-attempt-incident/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-attempt-incident/REPORT.md).
  M15 remains open; candidate selection is a user decision.
- The next bounded candidate, **completion-gate convergence**, was selected
  2026-09-03 by operator direction as the explicit recommendation of that
  diagnosis: both failing cells were completion-gate compliance tails on
  functionally-correct workspaces, and the diagnosis names how the model
  converts the basis-stamped repair stage into a converging tail. Core
  change (deterministic, no completion semantics change): a refused
  `task.complete` whose repair stage has no safe model-owned resolver
  (`operator_required`, including the operator-only diag fixture shape)
  escalates to an explicit **terminal** stage after
  `COMPLETION_REPAIR_TERMINAL_REFUSALS` consecutive refusals against the
  same basis — structured `terminal: true` /
  `terminal_surface: "ordinary_final"` plus a bounded instruction that stops
  re-proposing `task.complete` and ends with an ordinary final answer once
  the work is done. The escalated stage is durable (basis-stamped
  `CompletionRepairRecord`), visible to the next model decision, and resets
  when the basis moves. Matched gates: deterministic matrix (landed,
  `operator_only_refusals_escalate_to_a_terminal_surface`), and the full
  local gate plus recorded clean source with dual-platform CI is banked on
  `cc60194` (dual-platform CI run `33740918365` green, 2026-09-03). The new
  exact-source/product preflight **passed 2026-09-03** on clean HEAD `2adad31`
  (one `retry_policy_dev` normal cell, product surface, pinned PinAI tuple,
  explicit protocol; evidence at
  [`evidence/m15-preflight/`](../crates/agent-eval/evidence/m15-preflight/),
  source tree digest `ba2ec74c...`, verdict PASS). Next was at most one
  predeclared 12-cell window.

**Completion-gate admission candidate window (2026-09-03; predeclared clean
source `a6dc33e`, preflight PASS in the evidence commit, declaration recorded
before its run):**

- The 12-cell v4 window on the completion-gate convergence candidate + PinAI
  tuple was **predeclared 2026-09-03 before its run** (M15_ACCEPTANCE §7 item
  8): 3 fixtures × normal/resume × 2 repeats, the product surface (TaskProgress
  on, settlement and advisory candidates off, no counterfactual second
  request), the pinned serving tuple above (explicit `responses` protocol), one
  uninterrupted `agent-eval --m15-window` run whose cell directories landed
  under `crates/agent-eval/evidence/m15-window/_windows/1788438275930/`. The
  exact clean source identity was recorded at launch (`a6dc33e`); no source
  change happened during the run, the frozen-window rules of M15_ACCEPTANCE §5
  apply, and the mechanically regenerated report is the only accepted verdict.
- The window is a **valid FAIL: 10/12 pass, 0 NOT_RUN** — the mechanical
  report at
  `crates/agent-eval/evidence/m15-window/_windows/1788438275930/`. Behavior
  and allowed-diff pass 12/12; provider healthy in every cell; closures 2/12.
  Migrate 4/4 (7–11 rounds, clean continuation everywhere resumed); diag 4/4
  (all `active`, the completion-gate convergence closed the prior operator-only
  loop). The two failures are both `retry_policy_dev`: (1) `normal r2`
  exhausted the 48-round phase-one budget with `task.complete` refused 6/6 over
  a resolvable-looking `execution_debt` (`failed_commands remaining: 1`) plus a
  `task_progress`/`next_action_pending` tail — the ordinary-final terminal
  stage is deliberately not offered for such a model-resolvable blocker, so no
  `terminal_surface` fired and the model churned against persistent debt; and
  (2) `resume r1` failed at checkpoint restore with a Runtime
  storage-lifecycle lock-contention error (`lock workspace effects journal
  …workspace-effects.jsonl exclusively: … contested after 20 retries`) — an
  infrastructure failure, not model behavior. The cell records
  `runtime_error_class=runtime`; references to the evaluation harness in the
  diagnosis describe where the product restore was exercised, not an M15
  `harness_setup`/`harness_watchdog` classification. Post-window diagnosis at
  [`evidence/m15-diagnosis-completion-gate/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-completion-gate/REPORT.md).
  Per M15_ACCEPTANCE §5 the valid FAIL rejects the candidate; the window is not
  rerun. M15 remains open; candidate selection is a user decision.

**Workload disposition (2026-09-03; documentation only):** the proposed
"one-stop reliability" umbrella is a **large, multi-slice effort**, not one
bounded M15 candidate. It spans Runtime failure/closure semantics, Provider
buffering, eval CLI and environment preflight, CI, and frozen evidence identity.
No implementation, serving change, acceptance refreeze, preflight or live
window is authorized by this estimate. The small argument-order defect can be
fixed independently, but cannot repair or justify rerunning the valid 6/12
FAIL. Candidate selection in item 3 therefore remains open. The implementation
boundaries and relative sizes are recorded in
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md#2026-09-03-workload-split),
and the supporting hermetic gate-runner defect is
`EVAL-PREFLIGHT-01` in [`AUDIT_TODO.md`](AUDIT_TODO.md).

**Successor-source reliability repairs (2026-09-03; user-authorized after the
estimate above):** implementation proceeded as independent, reviewable slices
without changing or rerunning any formal M15 evidence.

- Re-reading the immutable `retry_policy_dev normal r2` stream corrected the
  latest diagnosis: the failed `cargo fmt --all -- --check` row was retired by
  its same-tool, same-`argument_digest` PASS. The surviving completion debt was
  the earlier speculative `fs.read src/job.rs` miss. Trusted unrooted
  `Observe/Search + PathNotFound` results now remain revision-bound negative
  facts without entering `failed_commands`; rooted, unattributed,
  target-mismatched and other failures remain fail-closed debt.
- Detached model calls capture only `ModelTransport`, not the whole
  `RuntimeServices`. A cancellation-lag regression proves actor shutdown can
  reopen the same workspace and acquire its exclusive effect-journal lock
  while the stale provider future is still alive.
- Eval parsing is complete before `eval.env` loads or any action starts:
  unknown/conflicting actions, duplicate selectors, missing/empty or
  option-shaped values, and trailing positionals fail without side effects.
- Python-backed tests and verification share one semantic resolver. Explicit
  configuration, `py -3`, `python3`, then `python` are probed with bounded
  time/output; Windows Store aliases fail as typed setup errors instead of exit
  9009. The resolved absolute invocation path preserves virtual-environment
  symlinks. The context-service integration suite is owned by its binary crate
  and uses Cargo's exact `CARGO_BIN_EXE`, eliminating stale-helper mtimes and
  build/touch workarounds.
- Buffered retry keeps the independent 16,384-chunk and byte limits unchanged,
  reports local capacity as typed non-retryable `LocalResourceLimit` rather
  than malformed provider data, and retains chunks linearly without quadratic
  coalescing.
- Final local validation used the repository's one-command doctor on the
  pre-recording source tree digest `73155555cc8e20cd…`: Python and the
  Cargo-owned helper passed;
  format, all-target/all-feature check, strict Clippy, build, and the complete
  all-target workspace test suite all passed. The Provider data-plane step was
  deliberately run without a key and skipped (pass/non-required), so this is
  a deterministic local gate, not a serving preflight.

The corrected diagnosis is appended to
[`m15-diagnosis-completion-gate/REPORT.md`](../crates/agent-eval/evidence/m15-diagnosis-completion-gate/REPORT.md).
These repairs are not a new M15 verdict: M15 remains open, the seven valid v4
FAIL windows remain immutable, and no formal preflight/window was run.

A valid formal failure rejects its exact candidate; only a typed `NOT_RUN`
permits rerunning that frozen window. Candidate selection is a user decision.

Post-M15 `LT-EVAL-06` development twins remain parked. The deterministic
`harness_maint_dev` fixture is available, but no TaskGraph, learned planner,
Context/GC retune, 27-cell expansion or 300×3 run is authorized. Self-Iteration
remains blocked by the governing milestone gates.
