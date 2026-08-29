# Audit follow-up

Confirmed defect queue. Only headings explicitly marked **open** or
**reopened** are actionable. This file still carries some clearly labelled
fixed/closed chronology that predates the compact archive; treat it as
non-actionable context and move it to the archive/git history when that section
is next touched. Do not reopen closed work under a new id without new evidence.

- Invariants: `AGENTS.md`
- Now/freeze/P0: [`STATUS.md`](STATUS.md)
- Execution: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
- Sandbox/M12: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md)
- Gates: [`ROADMAP.md`](ROADMAP.md)

M12/M13 must close before Self-Iteration. Do not add a database, vector
search, or learned ranking. Do not claim a milestone complete because
happy-path tests pass.

## Open P0 — trusted execution

The platform gates closed 2026-08-27 on clean-tree closure-audit evidence
(CORE-01/CORE-12 moved to the archive below). Residual OS isolation stays
outside the V1 availability floor — Linux UDP / raw / pathname-Unix, absolute
OS-level reads, Windows OS-level network, I/O bandwidth quotas,
seccomp / AppContainer — and `UntrustedGenerated` keeps failing closed on
native; making that profile runnable through WASI remains a V2 candidate.
Matrix: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md).

### CORE-10 — protocol remaining (not a transport swap)

`PLAT-00`–`PLAT-04` containment/protocol proof is landed. Remaining:

- PLAT-06 multiplexing (stay single-inflight in v0)
- PLAT-07 adapter envelope migration
- PLAT-08 Named Pipe/UDS (later)

Named pipes/UDS are not a fix for the platform gates. V1 still trusts Runtime
in the same address space.

## Open P1 — Tool Surface reliability

### PROV-LINK-01 — retryable transport failures killed runs instead of retrying (fixed 2026-08-26)

Live evidence kept dying on errors the provider layer itself marked
`retryable=true`: relay streams ended in `stream error: error decoding
response body` mid-round, and cells hung for minutes before surfacing it.
Two root causes, both fixed in `provider-openai`. First, the streaming
retry wrapper refused to replay once anything had reached the sink — the
right rule for a live UI listener, but an outcome-measuring harness has no
live listener to corrupt, so its runs had no recovery path at all. The
wrapper now has a buffering mode (`RetryingTransport::new_buffering`):
each attempt's chunks are collected internally and only a successful
attempt is forwarded, so every retryable transport failure replays from
scratch without duplication; the eval driver uses it. Interactive hosts
keep the live mode and its no-replay rule. Second, streamed bodies had no
idle bound: a silent peer held the turn open until the total client
deadline (or the peer) gave up. Both protocol paths now fail retryable
with a named stall error after `config.timeout` without bytes, resetting
on every delivered line. Deterministic coverage: three buffering-mode
tests (mid-stream replay with attempt stamping, non-retryable immediate
surface without leaking buffered output, exhaustion) plus a real-socket
stall test proving the idle bound fires long before the client deadline.

End-to-end validation landed the same day: re-running the OpenCode-relay
gate window after the fix, the cell that previously died on a relay
stream decode error replayed and completed (`strict=11/12 → 12/12`,
`usage_incomplete_cells=1 → 0`, lower-bound tokens gone; two cells carry
multi-minute walls as the visible cost of in-place replay). With
transport noise removed, applied-patch correctness is proven across two
providers and seven windows with zero wrong bytes ever committed, and
every remaining gate violation on either serving is model decision
behavior — chiefly the post-edit confirmation read. See
`tool-surface-edit-v3-clean-tree-2026-08-26-ox-r2/REPORT.md`.

### TOOL-CONTRACT-01 — optional-union and cursor semantics (deterministic fix landed 2026-08-24; live gate open)

The PinAI/Luna long-flow trace attributed 25/29 Dynamic failed outputs to
malformed pagination capabilities. A trial that silently mapped empty/zero
cursors to page one removed those failures but increased C from 75 rounds / 85
calls to 137 / 171 and created a 47-round turn; paired A timed out. A second
trial kept strict execution and published a cursor regex; the model then
fabricated matching-looking artifact identities, all 25 file/search calls
failed, C used 107/112 rounds/calls, and A timed out at turn 6. Neither trial
is accepted.

The retained surface uses `artifact.read` as the sole model-visible spill
continuation. `fs.list`, `search.grep`, and `code.symbols` return bounded first
pages plus a run-owned artifact ref and next line; their snapshot cursors stay
parser-only compatibility and execution remains fail-closed. This removes
three optional opaque-capability fields without adding a tool or prompt state.

The negative run also exposed `context.manage` parsing every property of its
union before `op` dispatch: unused empty UUID/enum fields invalidated valid
fetch/search requests. It now parses only fields consumed by the selected op,
publishes bounded kind/scope enums, and remains strict for required/relevant
values. `tool-runtime` unit tests and clippy are green. This is deterministic
tool-contract correctness, not a live convergence claim. The open gate is at
least two paired repeats with unchanged hidden success, lower median rounds and
calls, and no new p95/max-turn tail. Evidence:
`crates/agent-eval/evidence/longflow-pinai-luna-responses-2026-08-24/REPORT.md`
and `longflow-pinai-luna-cursor-normalized-2026-08-24/REPORT.md`.
The strict-schema follow-up is
`longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md`.

The same trace showed `task.complete` at 16 calls / 5 failures from invalid
model-supplied artifact claims. Runtime already merges its trusted current
assistant artifact and current verification refs into `CompletionRecord`.
The model schema now requests only the bounded summary; the artifact list is
parser-only compatibility and remains strictly validated when a trusted caller
uses it. A follow-up 4/4 pair reduced failures to 2/1 but left C at 77 rounds /
84 calls versus A's 49/36, so this deterministic defect is fixed while the
live round/call acceptance remains open.

### TOOL-CONTINUITY-01 — turn completion must not erase multi-turn task affinity (fixed; the CompletionOpportunity candidate ENDED by its 2026-08-28 decision-grade gate)

The one-shot completion trace isolated a lifecycle feedback loop rather than a
Context-size problem. Dynamic C called and committed `task.complete` on 9/15
turns versus A's 3/15. Each commit closed the task scope; the next user
directive started with a new task id and empty task-scoped `TaskProgress`, then
repeated capability discovery, list/read/search, and another completion.

The ended candidate separated implicit final-answer/turn completion from
durable task closure by making `task.complete` catalog-cold and leasing it for
explicit task-closure intent or a task requirement. Surface rev v5 later made
the compact schema always visible as a separate product decision. In either
surface, an accepted clean completion terminates without a confirmation model
round; failed siblings and invalid verification gates keep the recovery round.
This changes no Context/GC threshold or retrieval score.

Deterministic tool/runtime tests are green. A short live edit passed in 3
rounds / 2 calls / 0 failures. Two independent long-flow pairs reduced C to
49/44 and 57/52 rounds/calls (A 50/45 and 47/38) while median C input, selected
tokens, and resident bytes remained below A. Do not close this item yet: C
hidden success was 3/4 then 4/4 versus A 4/4 twice. The failed assertion was a
successful `RELEASE.md` edit that used `Version 2` instead of the requested
literal `v2`; it was not an edit/runtime failure, but success-neutrality is an
outcome gate, not a causal excuse. A later complete pair passed 4/4 in both
arms but regressed C to 82 rounds / 76 calls with one 30-round edit-repair
turn, versus A's 47 / 36. Task completions remained zero, so task continuity
fixed the identified lifecycle loop but is not sufficient for convergence.
Require broader clean paired repeats with no success or max-turn regression.
Evidence:
`longflow-pinai-luna-task-continuity-2026-08-24/REPORT.md` and
`longflow-pinai-luna-task-continuity-r2-2026-08-24/REPORT.md` and
`longflow-post-continuity-r3-2026-08-24/REPORT.md` and
`longflow-post-edit-anchor-r4-2026-08-24/REPORT.md`. The first post-hardening
pair passed 4/4 in both arms and restored C to 53 rounds / 51 calls / max-turn
7 versus A's 54 / 44 / 13, with no C failed outputs. This is one positive
dirty-tree pair; keep the item open for an independent repeat.

The first one-directive `retry_policy_dev` pilot exposed the opposite edge:
all four canonical cells ended without `TaskCompleted` and made no direct
`task.complete` call. Do not undo the continuity fix by making that tool
permanent. The body-free `CompletionOpportunity` candidate has landed
default-off and its deterministic already-satisfied replay is green. Two live
off/on attempts did not promote it; one proves the mechanism can execute an
offer -> lease -> explicit `task.complete` -> closure chain, but sparse arming,
worse paired efficiency and the reopened evaluator defects make the attempts
non-decision-grade.

The 2026-08-28 decision-grade rerun (post LT-RUN-05 WP1–WP5) armed once in
four on-cells and the leased decision did not call; the paired off baseline
closed a normal cell by itself. Outcomes regressed and closure did not
improve, so the frozen promotion rule ENDED the candidate — default-off is
final unless a new, separately documented design reopens it. The offer-debt
and verification-basis prerequisites both landed before the run, so the
failure is substantive (the affordance did not pay for itself), not
mechanical. See `evidence/opportunity-gate/REPORT.md` (2026-08-28 section).

Historical prerequisite text: an old mutation could combine with a newer
verifier and a crash could re-offer a key written after the last safe-point
capture; both are fixed (independent verification basis; `OpportunityOffered`
checkpoint debt with a serialization round-trip survival proof).

Surface decision (2026-08-28, rev v5): `task.complete` joined the always-loaded
production surface while `task.manage` stayed catalog-cold. The completion
acceptance gate structurally refuses premature or unverified proposals with a
typed per-turn warning. The invalid M15 v2 attempts observed many behaviorally
correct workspaces without closure, but they do not prove that discovery was
the sole cause or that v5 caused the later closure delta. Treat v5 as the
current product surface, not as an M15-promoted result; any future surface
comparison needs its own frozen paired gate.

### TOOL-PROC-01 — explicit ProgramResolver for process.run (fixed 2026-08-23)

Reproduction on Windows confirmed a tool-side semantic defect, not model
guessing: with `Command::new(argv0)` + `current_dir(cwd)`, a binary that
exists in the child cwd still failed to spawn under bare-name, `.\` and
`./` forms (CreateProcess does not search the child's cwd) while the
typed failure listing showed the same binary present — manufacturing the
exact contradiction that drives `foo` / `./foo` / `.\foo` guessing
loops. Landed: a host-owned resolver defines resolution explicitly —
absolute paths as-is; separator-relative forms join the call cwd (`..`
traversal rejected); bare names search the cwd first, then effective
PATH, PATHEXT-completed on Windows — and spawn always uses the resolved
absolute path. preflight, RetryDomain fingerprints and spawn share one
semantics; failures report the bounded candidate list they tried.

### TOOL-DIR-01 — transactional directory creation (fixed deterministically 2026-08-28)

The pinned-serving `retry_policy_dev` preflight exposed a general ACI gap.
`fs.write` correctly refused `tests/retry_policy.rs` because `tests/` did not
exist. The passing trace then required three further model decisions and the
sequence `capability.manage(shell.exec) -> shell.exec(New-Item) -> fs.write`.
An earlier trace also tried the PowerShell builtin through direct-argv
`process.run` first. This is execution/surface cost, not Context selection;
the passing cell accumulated 119,912 TurnFrame tokens versus 8,146 historical
Context tokens.

Do not make `fs.write` recursively create parents. Its current guarantee — a
file mutation may create the final file only inside an existing directory —
keeps directory topology inside an explicit approved and recoverable effect.
Landed semantics are deliberately narrower and safer than the original
multi-component sketch:

1. `fs.mkdir { path }` creates exactly one absent final directory component;
   its immediate parent must already exist. A multi-level path is an explicit
   sequence of effects, so a single approval never hides a partially-created
   `mkdir -p` chain;
2. the host binds the exact path to the existing `WorkspaceWrite` intent with
   zero content bytes. The workspace appends an authority-v3 `Prepared`
   record before creation, then creates relative to a pinned parent handle and
   commits the stable directory identity (Unix device/inode; Windows volume/
   file index);
3. rollback removes only the exact pinned, still-empty directory created by
   the transaction. Substitution, unexpected population, cleanup uncertainty,
   or a crash after create but before the committed identity is
   `Unknown`/`Ambiguous`, never false `NotApplied`;
4. an already-existing directory is an idempotent successful value with an
   explicit no-mutation fact. File collisions, missing parents, escaped roots,
   state-directory access and link/reparse traversal fail closed; and
5. `fs.write` keeps its existing-parent boundary but its typed missing-parent
   result now derives the first creatable component after the nearest existing
   parent and names the exact `fs.mkdir` call, instead of forcing a doomed
   deeper mkdir or a shell/process guess.

The authority reader remains byte-compatible with v1/v2 file frames. New
workspace, tool-runtime, host-policy and conformance tests cover durable
reopen, rollback, precondition races, the post-create crash seam, idempotence,
zero-byte containment and confinement. The tool is catalog-discoverable but
not yet on the default surface; deterministic completion does not by itself
prove a round/call improvement.

### TOOL-DIR-SURFACE-01 — choose directory-tool admission from paired evidence

Freeze the effect semantics above. Compare the current catalog-cold baseline
against one general recovery source that surfaces the exact host-owned tool
after a trusted `ResourcePath/path_not_found` result whose recovery contract
requires topology mutation. Reuse the existing bounded surface-source/
obligation machinery; do not parse free-form model text, create a permanent
task pin, or special-case an evaluator fixture. An always-ready compact schema
is a fallback candidate only if the recovery source still costs more decisions
than its per-round schema cost.

The deterministic gate is implemented and green (2026-08-28): after a failing
mutating result whose typed metadata names the first creatable directory, the
runtime derives a turn-scoped recovery request and surfaces exactly
`fs.mkdir` with `RecoverySurface` provenance for one decision. It proves
exact-tool provenance (report row origin, unit- and actor-verified), one-
decision source lifetime (consumed by the decision that saw it, never re-arms),
unload after consumption/directive end (the requirement dies with the turn;
lease reconciliation still releases it at the directive boundary), approval
unchanged (PreferSurface demand only; a read-only gate still refuses the
recovery-marked workspace write without dispatch), and no surface change for
unrelated missing reads (observation `path_not_found` carries no
`next_directory` and never arms). Covered by
`agent-runtime` unit tests (`recovery_surface_tests`,
`surface::tests::recovery_mark_*`) and actor tests
(`tests/turn/recovery_surface.rs`). The candidate ships behind a host switch
(`with_recovery_surface`, default off): the shipped product keeps the
catalog-cold baseline until the paired gate promotes it, so the two gate arms
differ only by that switch.

The live gate is `agent-eval --recovery-surface-gate [normal|resume]` (default
two repeats): the three representative packs (create-file retry, diagnosis,
multi-file migration) run normal/resume cells with the recovery-surface
candidate switch as the only variable; every cell records its setting in
dimensions.json. It is an isolated normal/resume paired comparison on
representative create-file, diagnosis and multi-file tasks (at least two
repeats per mode — full run is 3 packs × 2 modes × 2 repeats × 2 arms = 24
cells). Promote only with equal mandatory success, lower median
aggregate rounds and calls, no new max/p95 tail, and a reported
schema/prompt-token delta. Failed outputs remain counted. After the surface
choice, rerun one bounded source-bound product preflight before formal M15;
the earlier preflight did not contain this catalog entry.

The full 24-cell paired run completed on 2026-08-28 (clean tree at
`1a239479`, serving `gpt-5.6-luna` @ PinAI `/v1`, 128k, zero NOT_RUN), but a
post-run event audit changes what it can decide. Across all 24 event streams
there are zero `RecoverySurface` rows and zero `next_directory` recovery
facts. All eight `retry_policy_dev` cells instead catalog-loaded and called
`fs.mkdir` once successfully. The treatment was therefore never exercised:
the observed off/on differences are stochastic execution/order differences,
not attributable recovery-surface cost. The report also mixes final verdict
totals (off 8/12, on 7/12 because diagnosis is 0/8) with a 12/12 versus 11/12
success statement, and its table supports higher rounds in 3/6 pairs and
higher calls in 4/6, not 5/6 for both.

Decision: **NOT_EXERCISED / no promotion**. Keep the conservative
catalog-cold baseline and the `with_recovery_surface` switch off, but do not
claim the candidate caused the 55-round tail and do not advance the
always-ready fallback. Before another live comparison, report generation must
mechanically reconstruct event-derived counts and require non-zero candidate
exposure; an exposure-free run is inconclusive rather than a rejection.
Evidence remains immutable at
`crates/agent-eval/evidence/recovery-surface-gate/REPORT.md`; this audit
supersedes only its causal interpretation.

The run also exposed an evaluator-fixture defect independent of the surface.
`retry_diag_dev` fails 0/8 because the checked-in minimal/golden solution uses
`base << (attempt-1).min(63)`, which can wrap to zero, and the deterministic
self-check does not execute `m15_diag_oracle`. This is not evidence that the
serving missed a valid golden solution. Repair the golden implementation with
overflow-safe saturation, execute `cargo test --test m15_diag_oracle` in the
fixture self-check, then regenerate the pack digest before formal M15.

Calibrated 2026-08-29 (fixture authoring, allowed before source pin; it does
not change task or oracle meaning): the diag reference solution now widens to
`u128` before the shift so large attempts saturate at `max_delay_ms` instead
of wrapping to zero, the directive and `DIAGNOSIS` text name the large-attempt
saturation requirement, and the hidden check requires a `u128`/`leading_zeros`
overflow-safe marker rather than accepting the overflow-prone shift alone.
Fixture self-check now runs each pack's oracle against the untouched seed
(reject) and the scripted solution (accept) offline, and records both pack
digests as frozen constants. The calibrated diag digest is
`2fff51573097fe4c833215420dd0da74f11a645ef5c859bdd9bba87e5b427eeb`
(was `844793249406be591372f7ee8b17bd68b3933e9d2745988168de64834584aaf3`);
the migrate digest is unchanged at
`26d69fa1d4ccd00452b3ceb88f2a6ec7fbb977989df6d6f4e2f1e345660679cb`.

A 2-cell live smoke (`agent-eval --diag-smoke`,
[`evidence/diag-smoke/REPORT.md`](../crates/agent-eval/evidence/diag-smoke/REPORT.md),
PinAI `/v1` + `gpt-5.6-luna` + Responses + 128k) ran the same day: both cells
failed on the calibration edge with now-valid evidence. The model correctly
fixed the off-by-one and named `next_delay` in `DIAGNOSIS.md`, then wrote
`checked_shl(shift).unwrap_or(max)` — which only guards shift-amount ≥ 64, not
bits shifting out of the value (`100u64.checked_shl(62)` is `Some(0)`), so the
oracle's `next_delay(63, cfg(100,1_000)) == 1_000` still gets 0. Under the old
check table that fix would have passed every needle while failing the oracle,
reproducing the audit's complaint; the calibrated needle and oracle reject it
consistently. Keep the fixture as the M15 diag pack: a failing diag cell is an
honest reported fact, not a harness artifact. The
surviving blocker is a missing completion decision boundary; see
CONV-CLOSE-01 below.

The one-cell product preflight cleared 2026-08-29 (after Completion
Convergence V1 readiness): `retry_diag_dev` normal PASSed on the same pinned
serving at clean HEAD `09cce69` ([`evidence/diag-smoke/REPORT.md`](../crates/agent-eval/evidence/diag-smoke/REPORT.md))
in 14 rounds / 22 calls / 1 failed output / 139,886 ms with the hidden oracle
green, 6 durable checkpoints and settlement exposed (`seen`, pre 9/15 →
post 5/7) under ordinary-final closure — no `task.complete`, no auto-close.
The one unmatched diagnosis marker is the `backoff.rs` overflow-safe needle:
the written `exponent >= u64::BITS` + `checked_mul` + saturation shape passes
the oracle but not the reference `u128`/`leading_zeros` needle text, so it is
a needle-shape miss, not a functional failure. The calibrated fixture is
solvable on the pinned serving; the earlier smoke failures were the model not
solving the overflow edge. The resume arm passed the same one-cell preflight
same-day at clean HEAD `65f6cc8` (two resumed turns, 5 + 4 rounds / 19 calls /
0 failed outputs / 104,516 ms, hidden oracle green, settlement exposed
pre 8/19 → post 1/0, ordinary-final closure, the same single needle-shape
miss). Both one-cell preflight arms are cleared. M15 stays open until one
complete clean-tree 12-cell v3 window passes.

### CONV-CLOSE-01 — Completion Convergence V1 (deterministic foundation + exposure-qualified live gate ran 2026-08-29; efficiency open)

The 55-round / 129-call `retry_policy_dev` resume cell is the current bounded
readiness blocker, but `task.complete` itself is not established as its root
cause. The schema was present on every round. Across the 24-cell run it was
called 18 times and every tool result was successful; 17 calls reached a
`TaskCompleted` event, while one successful proposal did not reach a
`TaskCompleted` event within its trace. The long-tail cell made no
`task.complete` call and still performed formatting, linting and artifact
cleanup in its last rounds. Five other no-call cells ended their turn normally.

The stronger hypothesis is a missing completion decision boundary after the
last authoritative mutation and current verification, amplified by fragmented
verification and workspace noise (`target/` and `Cargo.lock` are model-visible
through `git.status` but evaluator-ignored). Implement the next task in this
order:

1. Make fixture cleanliness and allowed-diff visibility agree; generated build
   artifacts must not create model-visible cleanup work that the evaluator
   silently discards.
2. Add event-derived convergence metrics before policy: last authoritative
   mutation, current verification basis, first settled candidate, terminal
   mechanism, rounds/calls after settlement, outcome-free actions, and repeated
   read/diff/verify or artifact-cleanup actions.
3. Reuse the bounded `TaskRecord.resume: ExecutionState` and verification
   basis to derive dynamic states `Working -> VerificationDue ->
   VerifiedCurrent -> SettledCandidate`. No fixed round count establishes a
   state. A new mutation, obligation, failed/stale verification or unresolved
   constraint returns the task to `Working`.
4. At `SettledCandidate`, preserve model choice among an ordinary final answer,
   `task.complete` for whole durable-task closure, or one concrete remaining
   blocker/action. Runtime must not auto-close the task, suppress legitimate
   exploration or turn a subtask boundary into whole-task completion.
5. Prove deterministic scenarios first: ordinary final, durable closure,
   genuine remaining work, mutation after verification, stale verification,
   proposal settlement across cancel/resume, and cold resume. Run a small live
   gate with at least two paired repeats only after those scenarios and exposure
   accounting are green.

Landed 2026-08-29 (slices 1–5; the live gate is the remaining step):

- Slice 1 cleanliness: `ensure_workspace_git` now writes `.gitignore`
  containing `.focus-agent/`, `.gate/`, `target/` and `Cargo.lock`,
  matching the evaluator's allowed-diff skip policy, so build artifacts are
  no longer model-visible-but-evaluator-ignored cleanup work. Remaining
  slice-1 sub-items (mechanical event-derived report reconstruction and
  treatment-exposure accounting) stay open with the live gate.
- Slice 2 metrics: `RunMetrics` aggregates the first settled-candidate
  frontier event and reports `settlement: seen / pre_rounds / pre_calls /
  post_rounds / post_calls`; event-derived and unit-tested.
- Slices 3–4 dynamic state and decision boundary: `SettlementLabel`
  (`Working | VerificationDue | VerifiedCurrent | SettledCandidate`) is
  derived by `ExecutionState::settlement()` from verification validity plus
  the typed obligation ledger — never from fixed round counts; any new
  mutation, obligation, stale/failed verification or boundary change returns
  the task to `Working`. The label is published on `ExecutionFrontier` only
  on change, and `TaskProgressView.settlement` projects a bounded neutral
  one-liner only in the covered states; no stop instruction, no auto-close,
  no revival of `CompletionOpportunity`. The stale runtime comment describing
  `task.complete` as catalog-cold was removed (the v5 registry always loads
  it).
- Slice 5 deterministic proof: seven actor-level scripted scenarios over the
  real runtime are green — ordinary final, durable closure, genuine
  remaining work, mutation after verification (reopen and re-settle), stale
  verification (no re-settlement without a fresh verify), proposal
  settlement across suspend/resume (durable closure commits with a Current
  verification), and cold same-run restore (reopen and re-settle).
- Remaining slice-1 exposure accounting landed 2026-08-29 with the gate
  runner: the cell summary now carries event-derived settlement facts
  (`settlement_seen` plus pre/post rounds and calls), cell outcome lines
  render them, and the new `--conv-gate` runner (retry_policy_dev,
  normal/resume, at least two paired repeats) marks any cell with zero
  settlement exposure as inconclusive rather than a pass.
- The exposure-qualified live gate ran 2026-08-29 on the pinned serving
  (`crates/agent-eval/evidence/conv-gate/REPORT.md`): 4/4 cells PASS with
  4/4 settlement exposure and durable closure by the model's own
  `task.complete`. The settlement boundary is therefore verified under a
  real serving, including cancel/resume and cold restore.
- Read-only `--conv-tail` then sliced the post-settlement tail at the event
  level: the normal arm is clean (0 failed outputs, ≤4 `no_progress`
  deltas after the settled label); the resume median is driven by resume
  r1 alone, whose "long tail" is real phase-two development after an early
  seq-89 settled label (19 `advanced`, 4 Known mutations + 11 Unknown
  invalidations, 15 `no_progress`, 8 failed outputs) — each mutation
  returns the derived state to `Working` and the fresh verification
  re-settles it, exactly the designed behavior, and resume r2 is clean.
  The efficiency criterion therefore remains not claimed, but the tail is
  characterized as real remaining work plus retries in one cell rather
  than a settlement-boundary failure. Do not claim convergence or M15
  closed. Any future model-side treatment of the settlement projection is
  a policy/surface question outside this audit and must preserve the
  no-lost-work and no-auto-close invariants.

The bounded progress payload may retain only the current goal, unresolved
constraints, checked file identities/revisions (not file bodies), latest
verification basis/result, deduplicated known failed commands and one next
action. Every collection is capped and superseded by stable identity. It is a
resume/control summary, never an append-only transcript. Also remove the stale
runtime comment that still describes `task.complete` as catalog-cold; the v5
registry is the source of truth and always loads it.

This slice does not revive the failed `CompletionOpportunity`, add standing
prompt pressure, expand the transcript, retune Context/GC, introduce a
TaskGraph/learned planner, or specialize behavior for a fixture/provider. Its
promotion gate requires mandatory-success and resume-integrity parity, lower
post-settlement tail rounds/calls, no new max tail, and no loss of valid
unfinished-work continuation.

### Fingerprint v2 — preview ≠ identity (fixed 2026-08-23)

The old `resolution_fingerprint` hashed only the 20-name cwd preview and
serialized `env` as an unordered map. Landed: scope_key =
digest(cwd identity + effective PATH + resolver rules version) is stable
across epochs; fingerprint additionally digests the full bounded
directory state (all entries sorted, 4096-entry/128 KiB caps, truncation
flag hashed) plus canonically sorted env pairs. Beyond-preview changes
move the epoch; HashMap iteration order cannot.

### TOOL-EDIT-02 — canonical edit first-attempt success (confirmation met 2026-08-27; residual reliability work only)

The `v4` contract (byte-equivalent hunk decompositions accepted; byte/
revision/settlement truth preserved) reached its archival 4x3 confirmation
window: strict 12/12, gate 12/12, zero post-edit confirmation reads, with the
regression test locking the gate. No current consumer requires a golden
decomposition; reversing that choice needs a documented consumer first. What
remains is bounded reliability breadth (staged-byte accounting, external
races), not an open acceptance gate.

Do not reopen `TOOL-EDIT-01`: revision-aware exact refusals and bounded
candidates remain landed. The open gate is product reliability of the one
canonical mutation path, `edit.patch`.

Confirmed evidence: the 2026-08-22 replay of
`context-mech-convergence` found `edit.patch` 5/5 failed and
`edit.replace` 8/21 failed; all 11 multi-line `no_exact_match` refusals
were caused by `fs.read` showing LF text from a raw CRLF seed while the
edit tools matched raw bytes. Details and the non-retroactive reading are
in the evidence `REPORT.md`.

Landed implementation (not formal acceptance): uniform LF/CRLF-aware exact
matching with target-style preservation; constant-memory occurrence scans;
pre-allocation and workspace-boundary 4 MiB caps; bounded preflight reads;
duplicate resolved-target rejection; bounded missing-path/candidate output;
and one global multi-file echo cap. `fs.read` exposes a JSON-quoted path,
raw-byte revision and EOL facts (plus a bounded mixed-EOL token map). The model
sees only canonical revision-required `files[]`; the legacy single-file shape
is parser-only compatibility and cannot ambiguously bind one revision to many
files. A successful patch reports every new revision in submitted-file order
outside the optional echo cap.

The complete post-continuity r3 trace added a general recovery defect: a large
successful multi-hunk edit echo was prefix-only and hid the final changed file
tail, while model-visible ordinal `occurrence` allowed repeated low-uniqueness
`}` repairs to land on earlier braces. `edit.patch` now exposes only unique
exact anchors with enough unchanged context; ordinal input remains parser-only
wire compatibility. Both the per-file changed-span echo and the global
multi-file bound preserve head and tail with an explicit middle-omission
marker. `tool-runtime` 154/154 and `agent-eval` 129/129 are green. A short
post-change live smoke passed in 4 rounds / 3 calls / 0 failures with the first
patch committed and no confirm read or fallback. This proves model/schema
compatibility, not accepted long-flow performance; a new paired live repeat is
still required.

That paired repeat is now directional green: C 53 rounds / 51 calls / max-turn
7 versus A 54 / 44 / 13, both hidden 4/4, and no C failures or ordinal fields.
Three residual C calls read zero-byte successful verification artifacts. The
shared process output now withholds `artifact_ref` only when captured bytes are
zero and returns an explicit no-output terminal message; non-empty/truncated
captures are unchanged. The run predates that last correction, so no synthetic
call reduction is claimed. Require an independent post-output-change repeat.

That repeat validated the zero-output routing but exposed the next execution
coherence defect. Both arms passed hidden 4/4 and C made zero `artifact.read`
calls, yet C used 47 rounds / 44 calls versus A's 43 / 32. Evidence-only
results were 29 versus 16. In the largest amplified turn C already had an
exact current target body, but globally novel list/Git/catalog observations
each appeared to advance the global Evidence Frontier. Task-target relevance
now qualifies read-only frontier advancement once a directive has an exact
Fresh root: unrelated new facts stay stored and model-visible when selected,
but do not reset convergence debt. Directives without an exact root preserve
broad exploration and all warnings remain advisory. Selected exact file
bodies co-locate `workspace_identity=current`; tool schemas distinguish
`verify.run` recipe values from `capability.manage` tool names. The r5 trace
predates this correction, so require a new paired measurement. See
[`longflow-post-empty-artifact-r5-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-post-empty-artifact-r5-2026-08-24/REPORT.md).

The r6 measurement is a counterexample. Hidden success remained 4/4 in both
arms and C retained its Context advantage, but used 57 rounds / 56 calls /
max-turn 15 versus A's 49 / 38 / 7. The task-frontier advisory fired without
preventing a 15-round already-satisfied turn. Exact surface events show
decision-bound load churn: `git.diff` disappeared when the next decision
loaded `git.status`, forcing catalog reloads instead of allowing a cooperating
tool set. Runtime now keeps explicit model loads pending until exact use,
unload, or directive end, independently of one-decision called-tool result
delivery. This has deterministic cohort coverage and no Context/GC change, but
postdates r6 and is not live-accepted. See
[`longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md).

The r7 pair confirmed source lifetime but not convergence: the Git reload loop
disappeared and max-turn returned to 8, yet C used 62 rounds / 59 calls versus
A's 46 / 35. Eight of ten C catalog operations addressed `fs.write`,
`edit.replace`, `git.status` or `git.diff`. The isolated follow-up candidate
always surfaces only the compact universal subset `fs.write` + Git status/diff
(about 190 additional schema tokens; core about 947/4,096); `edit.replace` and
all non-core capability classes remain dynamic. See
[`longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md).

Three stable-core repeats support retaining that product boundary: r8 C/A was
46/46 rounds and 41/37 calls; r9 was 49/47 and 46/38, with hidden 4/4 in every
arm and one `capability.manage` call per arm. C retained 22–37% lower model
input and 61–65% lower historical Context. r9 still had a 9-round editor tail:
two sequential replacement hunks targeted the same ping-test anchor, so the
first consumed the second's match. `edit.patch` now makes operation intent
explicit (`replace` / `insert_before` / `insert_after`); insertions preserve
their unique anchor and omitted op remains parser-only replace compatibility.
The unchanged r10 live repeat passed hidden 4/4 in both arms at C/A 48/47
rounds and 41/39 calls, with identical three failed outputs and max-turn 8.
Explicit insertions were used successfully and the r9 Hello conflict did not
recur. C's remaining two patch refusals were safe ambiguous/no-exact locator
failures; neither was a filesystem settlement failure. Across r8-r10 the
median gap is +1 round / +4 calls. Retain the stable core and explicit
operations; do not admit positional or fuzzy matching from this one sample.
See [`r8`](../crates/agent-eval/evidence/longflow-stable-core-surface-r8-2026-08-24/REPORT.md)
and [`r9`](../crates/agent-eval/evidence/longflow-stable-core-surface-r9-2026-08-24/REPORT.md),
then [`r10`](../crates/agent-eval/evidence/longflow-explicit-edit-ops-r10-2026-08-24/REPORT.md).

Canonical batch path keys are acquired in sorted order before any edit
snapshot; one pinned bounded read feeds transformation, SHA-256, recovery hash
and bounded backup capture; the shared lease is retained through composite
settlement. Prepared content uses a short exclusive sibling temp, is checked
by open-handle/name identity, length and SHA before replacement, and the
installed target is checked before and after durable journal acknowledgement.
Unix mode bits or the Windows readonly bit are retained. Cleanup or rollback
journal uncertainty becomes `Unknown`; an already-landed replacement is never
reported as `NotApplied`. Mixed-EOL anchors use strict logical newline tokens
(`LF == CRLF` only), map an authorized match back to its raw UTF-8 span,
preserve physical EOLs by ordinal, and keep lone CR/non-EOL bytes literal.
Matching remains non-fuzzy and multi-file commit remains sequential with
honest partial recovery. `Effect::rollback` now returns `AgentResult<()>` as a
settlement claim: Workspace propagates cleanup/review/authority-terminal
failure; staged and composite rollback attempts every child in reverse order
and aggregates bounded diagnostics; Core installs its recovery fence and
Runtime emits `not_applied_cleanup_recovery_required` for commit-rejection
cleanup uncertainty and `execution_cleanup_recovery_required` for
preparation/execution cleanup uncertainty rather than treating either as an
ordinary rejection. Both projections discard proposed revisions/files and
retain only bounded attempted paths and diagnostics.

For Core-managed writes, authority journal v2 now lands its synced `Prepared`
intent before the deterministic `.fa-{tx_id}.tmp` is created. It records
before/after byte lengths and SHA-256 revisions. Reopen reconciliation limits
aggregate target/stage reads, refuses any file over the 4 MiB mutation bound,
and removes a staged entry only through a confined open handle after proving
regular-file type, full expected content, and name identity both before and
after hashing. Crash seams after intent persistence, stage sync, and review
record all have deterministic recovery tests; create collisions and partial or
substituted stages have fail-closed tests. Existing v1 records remain readable
with their legacy FNV-1a-64 evidence, under the same new byte bounds.
File mutations also refuse a missing parent before opening a transaction: they
may create the final file in an existing directory but never leave implicit
directory topology outside the approved/recoverable effect. Directory creation
will need its own effect contract if added later.

Post-fix evidence now exists. The source-bound, dirty-tree 2026-08-22 r4 run
used `agent-eval.tool-edit.v2` with the v3 gate over 4 fixtures × 3 repeats and
binds the implementation after rollback/recovery and filesystem P1 hardening:
12/12 raw-byte verification and 12/12 flow gate; 9/9 non-conflict first patch;
3/3 proactive stale routes; zero patch failures, forbidden fallback,
post-success confirmation reads, provenance/target/exact-hunk violations,
recovery-required or unknown settlements; 42 rounds. Total wall time was
164,417 ms and reported provider tokens were 258,325. It preserved all r3
correctness/call-quality results; wall p50/p95 were lower while token measures
were effectively unchanged. The gate independently binds calls to the frozen
task/pack/schema/source/model identities, the latest successful same-path
read, exact local-hunk fingerprints, raw final hashes, complete runtime
barriers, and the model-invisible stale-mutation boundary. See
`crates/agent-eval/evidence/tool-surface-edit-v2-diagnostic-2026-08-22-r4/REPORT.md`.

Confirmed residuals:

- the path lease is an in-process guarantee shared by clones of one
  `Workspace`, while a second official `Workspace::open` on the same root is
  refused by the authority-journal lock. Direct or authority-bypassing
  filesystem writers remain outside it, and hash→rename is not an atomic
  filesystem CAS against them;
- Unix still has narrow name/inode-check→rename and final-check→return windows;
  Windows preservation covers the readonly bit, not ACLs, alternate streams,
  hidden/system attributes or timestamps, and its parent-directory sync is a
  no-op rather than a proved power-loss barrier;
- `.focus-agent/changes.jsonl` is a serialized, flushed review log, distinct
  from the checksummed, synced authority
  `.focus-agent/authority/workspace-effects.jsonl`. Core-managed writes are
  mapped before temp creation, but a crash after authority `Committed` and
  before the review terminal can still leave review history at `Prepared`;
- the context-free `MutationTransaction::prepare` entry is retained for
  trusted tests/maintenance and is explicitly not crash-recoverable. A
  partially written, substituted, or colliding deterministic stage is not
  deleted automatically: reconciliation returns `Ambiguous` for manual
  recovery. Legacy v1 records still use FNV-1a-64, though their reads are now
  bounded; new v2 records use byte lengths plus SHA-256;
- mixed-EOL matching materializes one bounded canonical view per hunk; keep
  the simpler implementation until profiling shows it is a hot path, then a
  streaming token matcher may replace it without changing semantics;
- `fs.write` remains a blind whole-file upsert, now in the compact production
  core surface because create/replace is a universal coding operation and its
  78-token schema costs less than repeated catalog-control rounds. Execution
  is still effect/approval gated. A future compatible schema needs explicit create vs
  revision-checked replace rather than making it a second primary editor;
- the r4 live diagnostic did not exercise external-process races, process
  crash, disk-full/journal failures, or partial multi-file recovery, and does
  not yet aggregate staged bytes. Deterministic unit tests now cover three
  Core-managed prepare crash seams and conservative stage cleanup; real
  child-process fixtures cover abrupt kills at prepare, right after commit,
  and mid-batch (staged-byte frames verified intact), cross-process journal-
  lock races (refused second official writer, retry-window handoff), and
  mid-journal corruption plus checksum-valid sequence gaps. Portable
  disk-full injection has since landed behind the `test-faults` storage seam.
  A successful edit currently performs the snapshot
  plus repeated bounded full-file integrity hashes; treat that as a candidate
  measurement, not an established performance hotspot.

Before removing an integrity pass, add a test/benchmark-only counter with zero
production-path branching. For 4 KiB, 256 KiB and 4 MiB single/two/16-file
cases, report file-read bytes, SHA/FNV bytes, staged-write bytes, review and
authority journal bytes, file/directory sync counts, and replacement/changed-
span/journal amplification. The current nominal changed-file path has about
`2N + 3M` full-file handle reads for input `N` and result `M`; caching may make
physical I/O different. Fuse a pass only after measurement and only if stale,
staged-integrity, post-replace and final-ack truth remain independently
provable.

At the r4 stage, the next formal-acceptance blocker was a run of the same
frozen pack on a clean source tree; r4 deliberately used `--allow-dirty`, so
all manifests say `git_dirty=true` and `acceptance_eligible=false`.
Acceptance measures
non-conflict first-patch success, correct proactive/reactive stale recovery,
edit-to-passing verification, failure class, fallback-to-shell/`fs.write`,
confirm reads, rounds, tokens, p50/p95 latency, bytes read/staged, commit
conflicts and partial recovery. Safety refusals may be a separate class, but
remain in end-to-end task success/time/cost. Deterministic fault/race
coverage is landed through 2026-08-26, including portable disk-full
injection behind the `test-faults` storage seam (intent append, staged
bytes, committed record) with fail-closed recovery fixtures. M12 and M13
mainline does not move.

The first clean-tree frozen run (2026-08-26, PinAI/Luna) did not close the
gate and produced two findings instead. First, a real contract drift: the
unique-anchor schema made an explicit `op` required on every hunk after the
v3 gate was authored, so the model correctly followed its tool spec and the
gate rejected all 12 cells as non-canonical while strict raw-byte truth
passed 12/12. The gate now accepts exactly the runtime enum values
(`replace`/`insert_before`/`insert_after`) alongside the legacy omitted-op
spelling, with a regression test; the drifted run's bundle is archived out
of tree. Second, the post-fix rerun scored strict 11/12, gate 9/12,
non-conflict-first 7/9 over wall 1277 s (prior r4: 463 s): one cell lost
its provider session before any tool call (`usage_incomplete`, zero patch
attempts), one crlf cell spent its first attempt on revisions not from the
latest reads and recovered on the second, and one stale-recovery cell added
a post-edit confirmation read that this fixture's flow gate forbids. No
runtime regression is claimed — the residual failures are provider latency
plus model behavior variance. At that point TOOL-EDIT-02 still awaited one
stable provider window meeting 12/12 strict, 12/12 gate and 9/9 non-conflict
first patch; the v4 conclusion below supersedes that route.

A third run the same day in a normal-latency window (`tool-surface-edit-
v3-clean-tree-2026-08-26-r2/`) confirms the separation and narrows the
diagnosis: strict 12/12 again, gate 8/12, non-conflict-first 7/9, wall back
to 280 s with no session loss. Every applied patch across all 36 clean-tree
cells has been byte-perfect; the four gate failures were two post-edit
confirmation reads this fixture's flow contract forbids, one stale-revision
first attempt (recovered), and one non-exact first-hunk attempt (recovered).
No raw-byte mismatch was observed in these frozen cells; what fluctuates
between provider windows is first-attempt decision quality. This is evidence
for the bounded pack, not proof of an editor-engine property in general.

A fourth run the same day (`tool-surface-edit-v3-clean-tree-2026-08-26-r3/`),
launched immediately after a green single-cell availability smoke in a
healthy window, reproduces the pattern once more: strict 12/12, gate 9/12,
non-conflict-first 8/9, wall 570 s with no session loss. The three gate
failures are two post-edit confirmation reads forbidden by the
stale-recovery flow contract (the same fixture passed cleanly with zero
confirm reads on its third repeat) and one non-exact first-hunk attempt
recovered on the second. Across all four clean-tree runs every applied
patch has been byte-perfect; strict raw-byte truth has never failed except
the one cell whose provider session died before any tool call. The gate bar
(12/12 strict, 12/12 gate, 9/9 non-conflict-first) remains unmet by this
provider serving; at that point the item still awaited a window where the
served model's first-attempt discipline held across all twelve cells.

A fifth run the same day (`tool-surface-edit-v3-clean-tree-2026-08-26-r4/`),
in the fastest window yet (wall 218 s, no session loss), reproduced the
verdict shape exactly: strict 12/12, gate 9/12, non-conflict-first 8/9,
with the three gate failures again being two forbidden post-edit
confirmation reads on the stale-recovery fixture and one non-exact
first-hunk set recovered on the second attempt. Five independent windows
have now produced byte-perfect applied patches in every cell that reached
a tool call, with gate failures drawn from the same two model-behavior
shapes. The diagnostic has saturated: further same-day retries against
this serving added no information, so the next historical step was a materially
different provider/model serving rather than another same-window attempt.

That materially different serving ran the same day: one clean-tree gate on
the local OpenCode relay (`ox-alpha-free`, availability precondition green,
`tool-surface-edit-v3-clean-tree-2026-08-26-ox-r1/`) scored strict 11/12,
gate 6/12, non-conflict-first 8/9. The strict miss and one cell's transport
death were a relay stream decode failure after two rounds; every completed
cell applied byte-perfect patches, and the model showed perfect hunk
discipline — no non-exact first attempt and no wrong-revision selection in
any window. Its behavioral failures are narrower than Luna's but heavier:
all five are the same forbidden post-edit confirmation read (5 of 11
completed cells). Cross-model summary after six runs: every completed
frozen-pack cell was byte-correct, while first-attempt flow discipline varied
and both servings repeated the post-edit confirmation read. This separates
the observed failure classes without generalizing the finite pack into an
engine-wide proof.

Root cause of the binding violation (event-level audit of
`mixed_eol/r1` in the ox-r2 window): after a successful patch whose echo
already carried the full committed post-state and the new revision, the
model issued a second `fs.read` returning byte-identical content and then
narrated "**Result verified:**" over lines it already possessed — a
trained-in verify-after-mutate habit, not an information need. The rule
forbidding it lives only in grader config (`max_confirm_reads_after_success:
0`); no model-visible surface states it. Surface archaeology shows the
contract used to be visible: the patch tool description carried "so
chained hunks need no confirm re-read" until the v3 surface compaction
dropped it under that tool's 96-char description cap, while `edit.replace`
still carries its twin sentence today. Fix: state the contract on the
success echo itself ("patch applied and committed; this echo is final,
no re-read needed"), which costs ~15 tokens on successful patches only
and leaves the 96-char schema cap intact.

Validation on the same relay serving, clean tree at the fix commit
(`tool-surface-edit-v4-clean-tree-2026-08-26-ox-r2/` and `-ox-r3/`, both
with REPORT.md): strict 12/12 in both, and post-edit confirmation reads
are gone — 0 of 24 cells across the two archived windows, versus every
prior window on either provider producing them. Wall time dropped from
871 s to 509/594 s with rounds 46 → 42, the visible saving from the
eliminated confirmation round-trips. A first same-day window ran before
these two with flags mis-ordered (`--evidence-dir` after
`--tool-edit-run`) so no artifacts persisted; its console verdict was
strict 12/12, gate 12/12, non-conflict-first 9/9 — observed but not
archival evidence. The remaining bar-blocker is now solely the
`batch_two_file` exact-hunk cell (~1 of 3 repeats per window): the model
merges each file's two anchor lines into one multiline hunk; bytes are
always correct and `confirm=0`. The chosen product contract is committed
byte/revision/settlement truth: hunk partition is not model-visible authority
and no current consumer requires the golden decomposition. `exact_hunks` is now
versioned to accept byte-equivalent decompositions while retaining paths,
strict bytes, revisions, settlement and no-fallback/no-confirm checks; the gate
is `agent-eval.tool-surface-edit.v4`. A future canonical-granularity rule
requires a documented consumer before reversing this decision. One archival 4x3
confirmation window on the versioned gate is now landed:
`tool-surface-edit-v4-clean-tree-2026-08-26-r4` scored `strict 12/12 gate
12/12 non_conflict_first 9/9` with zero confirmation reads. TOOL-EDIT-02 now
meets its product contract on the frozen surface.

## Open P1 — runtime scheduling correctness

Design + invariants: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
("Lifecycle clocks and maintenance scheduling"). The tool-lifecycle clock
defect (load/execute advancing the shared tick), the O(R²) repeated
tool-scope closes, and the every-round `BeforeModel` minor scan are
**fixed**; their write-ups live in that section.

### SCHED-01 — BeforeModel runs a full minor scan every round (fixed 2026-08-23)

Measured: 77 `BeforeModel` maintenances per 15-turn cell, most with no
pending state change (`gc_work_batch` 4096 ≫ heap size, so each pass
rescans Resident+Warm). Landed: the engine stamps `last_maintained_seq`
at each completed pass, so `BeforeModel` at an unchanged sequence is a
true no-op — default report, no scan, no sequence consumption;
lifecycle-closure triggers always run. Bounded dirty batches
(`MaintenanceDebt`) remain the later step before touching scan width.
CPU/lock/event work — no extra-round causality claimed.

### SCHED-02 — search candidate completeness contract (fixed 2026-08-23)

The shared index bounds tokens/doc (64), postings/token (4096) and body
text to its first 512 chars, while candidate hits suppress the residual
scan — deep-body keyword recall was not guaranteed end-to-end. Landed:
catalog search returns `SearchCandidates { ids, incomplete }` with
`SearchIncompleteReason::{SaturatedPosting, TruncatedIndexedText}`;
an incomplete set triggers one bounded residual verification of the
non-candidates against full stored bodies (lazy projection keeps memory
at O(limit)). Search is GC's safety net; recall completeness is now
explicit, not implied.

### SCHED-03 — convergence failure-cluster escalation (fixed 2026-08-23)

Invented-program PathNotFound streaks survived per-call cwd listings (8
attempts across 4 spellings) because every spelling is its own
signature. Landed: alongside the MOD-PROG-01 identical-signature
counter, `ExecutionState` aggregates consecutive same-class failures
across different targets over an unchanged world; at ≥2 distinct targets
the TASK PROGRESS view carries an EXECUTION STALL line naming tool and
class (advisory only — the model still chooses). A class change, any
world progress, or an Evidence-class observation restarts the cluster;
the per-signature threshold stays at 3.

### SCHED-04 — reread motive attribution instrument (instrument landed 2026-08-23)

Latest long-flow: fs.read 21 / repeats 18 with Warm=Stored=0 — rereads
are descriptor-only (12) and needs-revalidation (7) motives, NOT Context
GC reclaims. Landed: the `protocol-checkpoint-body-missing` motive class
identifies identity-only reads of a body the model already consumed
(read-provenance fact, unchanged digest, descriptor residency), split
out of descriptor-only/needs-revalidation so a protocol body cache would
be sized against real demand. Residency loosening stays rejected on
current evidence; the tiny current-turn LRU gets built only if this
motive shows up in live runs.

### EXEC-REV-01 — verification basis diverges across consumers (fixed 2026-08-27)

Landed: `TaskAnchor.verification_revision` is an independent basis (serde
default) bumped only by authoritative boundary changes — original goal,
constraints and acceptance criteria — while progress/open-loop/next-action
maintenance advances only the whole-record CAS revision; `TaskManager` syncs
`ExecutionState.verification.spec_revision` to the basis, and facts, exact
verifier sources, freshness/validity and the opportunity key
(`opp/{task}/a{basis}/d{directive}/w{workspace}/...`) all read the basis.
Per `LONG_TASK_EVALUATION.md` Slice B, accepted criteria move the basis too
(they are the authoritative verdict) while model-derived criteria remain
proposals — `task.manage` cannot submit the field, so no criterion-level
approval gate is implied. `validity()` additionally refuses Current when the
last evidence row is bound to a different basis than the live one, so no
consumer can disagree if the basis ever moves without the `SpecChanged`
side effect. One cross-consumer regression covers progress-only movement,
an acceptance-criteria change and a checkpoint round-trip, asserting that
ActiveTurn validity, completion, exact reuse and the derived completion
opportunity agree in all three phases. Residual, tracked with the
cold-resume matrix: the persisted offer key must also accrue checkpoint debt
(it does since 2026-08-28) with a crash-window proof that once-per-basis
discipline survives recovery.

Original observation: a progress-only CAS advances the task/execution anchor revision
without marking the existing verification stale. `ExecutionState::validity()`
and completion therefore accept it as Current, while exact reuse and
`CompletionOpportunity` require the fact's old anchor revision to equal the
new record revision and reject the same PASS (`agent-runtime/src/task.rs`,
`execution/state.rs`, `opportunity.rs`).

Risk: completion, resume/reuse and advisory closure do not share one definition
of current verification. The existing test proves only the enum remains
Current; it does not prove agreement among those consumers.

Required fix and exit evidence: introduce an independent verification-basis
revision or digest while retaining the whole record revision only for CAS.
Define criterion origin/authority, bind facts to the shared basis tuple, and
use one currentness predicate in ActiveTurn, task resume, completion, exact
reuse and opportunity derivation. Cover progress-only movement, authoritative
boundary movement and checkpoint round-trip in one cross-consumer regression.

### CONV-01 / CONV-02 / PROTO-EVID-01 — closed 2026-08-23

All three landed in Execution Convergence V1 (see
[`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md) and
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)); write-ups moved to
the second-round section below and to the closed archive. The remaining,
narrower residuals are CAP-OBS-01 and CONV-03 there.

### LONGTASK-03 — acknowledged checkpoints are not uniformly cold-restorable (reopened 2026-08-27)

Landed toward this fix 2026-08-27: automatic safe points, instance
checkpoints and terminal completion share one assembler with the bounded
capability-generation handshake; snapshots validate before persisting;
terminal completion is two-phase (durable prospective-terminal acknowledgement
before any in-memory commit or `TaskCompleted`, failed writes leave the task
pending/retryable); store input/output bounds and bounded retention landed.
Fixed 2026-08-28: acknowledged artifacts — including the final terminal
snapshot — cold-load, validate and restore into fresh Runtime instances in
the deterministic gate (third-instance phase), with the capability plane
handshake-verified and its generation published on every acknowledgement.

Observed: completion clears `TaskManager.active`, then captures the final
checkpoint while the actor still carries `current_task_id`; restore validation
rejects that mismatched authority state. The write path can acknowledge the
artifact without first calling checkpoint validation. Automatic safe-point
capture also does not reuse the bounded capability-generation handshake used
by external instance checkpoints (`agent-runtime/src/actor/turn.rs`,
`actor/safepoint.rs`, `checkpoint.rs`, `instance.rs`).

Risk: `CheckpointDurable` can name an artifact that no fresh Runtime can
restore, or a torn cross-plane snapshot. Whole-file load, serialization and
retention are also not bounded for a genuinely long run.

Required fix and exit evidence: route all checkpoint writes through one
capture -> stable-generation merge -> validate -> persist path. Use two-phase
completion: prepare a prospective terminal snapshot with both task authorities
cleared while the live task remains pending/retryable; acknowledge it before
committing in-memory completion and emitting `TaskCompleted`. Reject oversized
header/payload/artifact input and define bounded local retention. Load and
restore every acknowledged test artifact into a fresh Runtime and Context,
including the final artifact; stress `configured_retention_limit + 2`
checkpoints and the byte ceiling without deleting the latest required, pinned
or referenced recovery artifact.

### LONGTASK-04 — resume durability watermark is not a snapshot fence (reopened 2026-08-27)

Landed toward this fix 2026-08-27: actor-owned monotonic snapshot sequences
replace the anchor-aliased watermarks (`agent-runtime/src/actor/safepoint.rs`),
acknowledgements retire exactly their artifact's debt set, continuation
requires no outstanding debt / no in-flight write / no failed write plus a
landed sequence, and the allocator watermark rides the checkpoint lineage
without moving backwards on restore. Fixed 2026-08-28: the deterministic
gate's resume phase consumes only the acknowledged artifact tuple
(artifact, checksum, sequence, capability generation) through the verified
cold-load path, with third-instance terminal restore closing the matrix.

Observed: `resume_state_revision`, required revision and durable revision all
alias the task-anchor revision. Workspace mutation and verification can change
the checkpoint without advancing it; task changes can lower it; zero also
means both "none" and a real first revision. The continuation gate compares
only numbers and does not require no debt, no failed write and no in-flight
write. A newer debt may also appear while an older write is awaited
(`agent-runtime/src/actor/safepoint.rs`).

Risk: an older durable acknowledgement can satisfy a newer state, including
after the newest write failed.

Required fix and exit evidence: add an actor-owned, monotonically increasing
snapshot sequence independent of task anchors and persist it across the
existing durable Runtime lineage. Bind each acknowledgement to lineage +
sequence + artifact + checksum, and allow continuation only when there is no
debt, failed write or in-flight write and `durable_sequence >=
required_sequence`. Cover same-anchor distinct snapshots, out-of-order ack,
new debt during an old write, revision zero, task switch and failed-write
retry.

### EVAL-05 — resume twin does not prove the latest settled cold artifact (fixed 2026-08-27)

Landed: the harness correlates the checkpoint artifact with its snapshot
sequence and checksum across the resume boundary — a sequence mismatch fails
harness-side before restore — and tracks the full durable tuple
(artifact, sequence, checksum), requiring the durable-after-mutation ack to
match the last resume-committed sequence.

Original observation: the harness now builds a fresh engine and uses the verified loader,
but treats any `CheckpointDurable` arriving after a mutation as the mutation's
checkpoint. A pre-mutation capture may acknowledge after a later write; the
cross-phase state retains only the artifact name, not the acknowledged
sequence/checksum/capability generation (`agent-eval/src/long_live.rs`).

Risk: phase two may restore an older Runtime snapshot while the workspace is
already ahead, so the current cold-resume and continuation dimensions are not
decision-grade.

Required fix and exit evidence: correlate the latest `TaskResumeCommitted`
with its exact lineage, sequence, artifact, checksum and capability generation. Cancel
only after that tuple is fully settled; carry only that tuple across phases;
reject an older or mismatched acknowledgement before restore.

### EVAL-06 — isolated oracle setup failures are misclassified as behavior failures (fixed 2026-08-27)

Landed: the harness pre-creates the tests directory, classifies setup and
injection failures as typed `not_run` instead of behavior fail, runs the
workspace self-check before oracle injection with a distinct cargo target so
the oracle cannot pollute it, and removes the injected oracle after the run.

Original observation: oracle injection writes under `tests/` without first creating the
directory. Four retained `behavior=fail` records are actually setup failures
(`os error 3`), not executed behavioral assertions. The injected oracle also
overwrites the recorded cargo argv, blurring it with the workspace self-check
(`agent-eval/src/long_live.rs`).

Risk: setup and harness failures are charged to model behavior and can support
an invalid promotion conclusion.

Required fix and exit evidence: pre-create harness-owned paths, keep oracle
and workspace-self-check commands separate, and run the self-check before
injection (or through a target proven to exclude the oracle). Remove the oracle
before any later self-check. Type setup/start failures as `not_run(reason)`.
Only an oracle that actually executes may produce behavior PASS/FAIL; add pass,
assertion-fail, missing-directory, accidental-self-check-inclusion and
spawn-fail fixtures.

## Second-round review 2026-08-23 — trust & obligation

Source: the 2026-08-23 post-convergence full-repo review
([`TRUST_AND_OBLIGATION_TODO.md`](TRUST_AND_OBLIGATION_TODO.md), items
4–31). Clean n=2 longflow evidence: all four arm-runs passed hidden
verification, but C r2 rebuilt a process-guessing chain while global
frontier metrics stayed under threshold — progress-as-global-scalar
cannot see blocker debt, and three correctness/trust holes were
confirmed.

### PROMPT-AUTH-01 — restored raw body entered System role (fixed 2026-08-23)

The protocol body cache rehydrated file bodies through the runtime Focus
frame, which renders as a System-policy message — elevating attacker-
influenceable file content above the operator's instructions. Landed:
RESTORED TURN BODIES now ride the user-role context frame; regression
test asserts restored content never reaches focus_frame/system_policy.

### EXEC-EVID-01 — resource evidence had no currentness bound (fixed 2026-08-23)

Resource-validity evidence rows stayed visible after their file changed
(the digest was checked only against a fact table that the mutation
itself had already refreshed). Restore trusted no bounds on the new
convergence fields. Landed: one `evidence_is_current` predicate shared
by projection and sweeping (WorkspaceRevision equality; Resource needs a
Fresh fact at the identical digest, evaluated after this round's facts
land; Turn requires the current turn); `validate_execution_state`
additionally rejects oversized evidence/deltas/targets/obligations and
per-row string overruns, so restore cannot trust an unbounded checkpoint.

### CAP-OBS-01 — dynamic producer metadata must not become trusted execution facts (open, narrowed)

`ToolOutput::file_path/file_revision/resource_touches/is_verification/
may_mutate_workspace` read producer-stamped metadata, and
`take_runtime_diagnosis` read producer `failure_class`/`recovery_hint`.
Operator-trusted builtins are fine; a dynamic capability could forge
`path`/`revision`/`verification`/`mutates_workspace: false` and feed
ResourceFact, Verification, WorkingSetSignal, TASK PROGRESS authority.
Landed 2026-08-23: fail-closed routing-layer sanitizer
(`sanitize_untrusted_producer_output`) strips reserved diagnosis keys
before Core reads them plus the producer-authority keys from every
capability output; contract direction written into
[`TOOL_RESULT_ENVELOPE.md`](TOOL_RESULT_ENVELOPE.md). Typed-facts
substrate landed 2026-08-26: `ToolExecutionFacts` in
`agent-contracts/src/execution_facts.rs` carries resources / mutation
bound / verification stamp / runtime diagnosis as typed values with
constructors that mirror the legacy accessors exactly and default to
empty facts; no consumer reads them yet and there is no durable wire
form. First lane landed 2026-08-26: `ToolDispatcher::execution_facts`
(default empty) lets the operator-trusted builtin registry translate its
own stamped outputs at one sanctioned point inside the trust boundary,
capability-routed results contribute empty facts by routing, and the
actor's body-free batch ledger now consumes those facts instead of
re-deriving from producer metadata. The turn frame carries the same facts
on every tool-result step (`Option<Box<_>>`, serde-defaulted so old
checkpoints restore as `None`), and the prompt's fs.read body-identity
hints read them with a legacy-metadata fallback exactly for `None` frames.
Heating landed 2026-08-26 as consumer-side adoption only — trusted
handlers still stamp metadata keys, so no model-visible output shape
changed: `ContextIngress::ToolObservation` carries `facts`
(`Option<Box<ToolExecutionFacts>>`, serde-defaulted so old service frames
restore as `None`); the actor forwards the turn-frame step facts at its
single observation-ingest site with zero extra dispatcher calls, and the
context engine reads heating/observation identity facts-first with a
per-value legacy-metadata fallback for frames without captured touches.
Still open before
Self-Iteration: move fact construction into individual trusted handlers,
and define the event-level durable wire form. Verification's
representation landed 2026-08-26: the no-attribution frontier entry now
reads its verification claim from the dispatcher-lane facts with a
per-value legacy fallback, while `observe_tool_attributed` keeps
pre-dispatch attribution as the only reusable-verifier authority —
producer metadata can no longer mint even the compat path's claim when
facts are present. Sequencing note: once trusted handlers
stop stamping
metadata keys entirely, the legacy derivation returns empty by
construction and every ingest/prompt fallback becomes removal of dead
code instead of a dual-path migration; removing the stamps changes
model-visible tool output shapes (pinned/convergence behavior could
shift), so consumer-side adoption deliberately keeps them for now.
Verification needs no behavioral change: the
production observation path already draws verifier authority from the
trusted pre-dispatch attribution channel; only its representation differs.

Handler-level direct construction landed 2026-08-26 as a dual-write slice
with no model-visible change. The authoritative builtin handlers —
`fs.read`, `fs.list`, `fs.write`, `edit.replace` (success, no-op and
refusal outcomes), `edit.patch` (applied, no-op, refusal), and
`verify.run` (which owns the verification claim over its wrapped process
result) — now stamp native `ToolExecutionFacts` at construction time
under the reserved `metadata._execution_facts` key;
`sanitize_untrusted_producer_output` strips that key together with the
other reserved keys, so a dynamic capability cannot mint facts by
carrying it. `BuiltinToolDispatcher::execution_facts` prefers native
stamps for owned tools and keeps the legacy key derivation as the
fallback for handlers that have not migrated; per-handler tests assert
native equals derivation on every outcome shape, so consumers switching
between channels see identical values. The legacy stamps stay because
removing them changes model-visible output shapes.

Producer-bound coverage completed 2026-08-26 the same day: every
remaining builtin family now stamps an explicit workspace-mutation bound
on its own outputs (`shell.exec`, `process.run`, `git.status`, `git.diff`,
`search.grep` including cancellation and no-match outcomes,
`code.symbols`/`code.diagnostics`, `artifact.read`,
`context.manage` across all ops, `task.complete`, `task.manage`, and the
`capability.manage` control surface), mirroring the temporary
builtin-name table exactly so the two channels cannot disagree. Shared
refusal helpers (`hidden_path`, missing-path) stay on the derivation:
they serve many tool names and their outputs carry no authority keys.
`process.session` now stamps `may_mutate=true` natively on every action (`start`/`poll`/`stop`) via the `_execution_facts` channel, and the legacy name-table fallback also resolves it to true — both channels agree on the conservative `Unknown` footprint shared with `shell.exec`/`process.run` (see [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) resolved `process.session` bound). With coverage this complete, retiring the name table reduces to retiring the legacy stamps once model-visible drift is acceptable (derivation then returns empty by construction and every fallback becomes dead-code removal), plus the event-level durable wire DTO. The session-bound decision is now closed.

### PROTO-EVID-02 — cache correctness + observability (fixed 2026-08-23)

Two findings: edit echo was cached as if it were the exact body (it is
a patch echo, not the file), and "remaining rereads are not cache
misses" was unverifiable because no counters existed. Landed:
`record_protocol_body` accepts fs.read bodies only (edits invalidate
their paths); assembly emits per-round `ProtocolBodyCacheStats`
{eligible, hit, miss, invalidated, oversize, restored_body_tokens} which
agent-eval aggregates into summary.json — hit rate is now independently
verifiable from any bundle.

### CONV-03 — obligation lineage + precondition epochs (landed 2026-08-23, second refinement)

Global frontier advance does not prove blocker resolution. Landed in
two steps: first the typed `ExecutionObligation` ledger with host-
trusted `resolution_fingerprint` preconditions and bounded UNRESOLVED
BLOCKER warnings; then — after the live run showed attempts never
escalating — the lineage model. `ExecutionObligation` now carries a
stable `scope_key` (ExecutableResolution = resolver-context digest;
path or target identity for the other domains), a per-epoch
`precondition` fingerprint, `epoch`, per-epoch `attempts`, and cross-
epoch `total_attempts`. Same scope + same fingerprint accumulates; same
scope + new fingerprint advances the epoch (**PreconditionChanged ≠
ObligationResolved**); resolution requires blocker-specific proof — an
ExecutableResolution obligation is cleared only by a success carrying
the *same* scope_key *and* fingerprint (a successful rustc build no
longer clears "compiled tests exe not found"), while EditTarget /
ResourcePath / ProjectMarker keep their target-specific proofs. Hard
refusal stays exactly as narrow as before: provably equivalent retries
only. The LaunchResolutionFact revision guard note remains: it is
deliberately conservative until fingerprints are recomputable
pre-dispatch without I/O.

### CONV-OBS-01 — obligation lifecycle is event-visible (fixed 2026-08-23)

Bundles could not prove whether blocker warnings fired. Landed:
`RuntimeEvent::ExecutionObligation {kind, domain, scope_digest, epoch,
attempts_in_epoch, total_attempts}` with kinds opened / attempted /
precondition_changed / resolved / dropped, emitted from the observation
pipeline; agent-eval aggregates `obligation_*`,
`avoidable_failure_calls` (failures after the first in one lineage),
`max_obligation_attempts_per_epoch`, `max_total_attempts_per_lineage`,
and per-user-turn tail metrics (`max_turn_rounds`, `p95_turn_rounds`)
so optimization targets long turns, not task-round means. Deferred
honestly: a `warning_surfaced` kind needs the render path to report
surfacing; do not fake it from attempt counts.

### CONV-04 — execution attribution + capability leases (partial; attribution/negative-fact/equivalence-class/obligation-provenance slices landed through 2026-08-26)

The retained long-flow event streams prove that the current convergence
scalar is the wrong decision signal for optional exploration. C/A produced
the same eight successful Known mutation outcomes, but C produced 48 versus
21 evidence-only results, 9 versus 0 Unknown invalidations, and an 18 versus
3 maximum result streak without an outcome advance. Its catalog-loaded
optional surface exposed 134 reported rows (118 unused in their round) and
received 18 requests; A exposed 28 (26 unused) and received two. All selected
reports were untruncated. The 36-call C-A difference is exactly the additional
27 evidence-only plus 9 Unknown results in this diagnostic.

Landed measurement foundation: `agent-eval::RunMetrics` aggregates the
body-free `outcome_frontier_*` partition and bounded
`catalog_optional_*` exposure/request join from existing events, renders them
under `--metrics`, and includes them in new bundle summaries. It counts
`TransientNoPersist` results, but does not persist their bodies or change
Runtime behavior. Source-bound facts and the causal trace are in
`crates/agent-eval/evidence/longflow-task-provenance-2026-08-24/REPORT.md`.

Landed first behavior slice (not yet a live performance claim): Runtime uses
source-driven schema leases rather than a round TTL. Exact tools called by one
model decision remain rooted through execution and the next successful
decision; reuse renews the result-delivery source, non-use releases it. A
trusted catalog-load receipt establishes a separate pending-use source until
the exact tool is called, explicitly unloaded, or the directive ends. Adjacent
loads therefore form a small turn-local cohort instead of evicting each other;
using one consumes only that member. New directives clear leftover ephemeral
leases, while explicit task requirements and typed verification/evidence roots
survive. Host/operator loads are a distinct persistent source until explicit
unload; Runtime/model load paths never become task-global pins, and restore
unions current composition sources with checkpoint residency without
promoting restored-only rows. `ExecutionBatchSettled` accounts
transient/refused/reused actions without persisting their bodies. Oversized
batches execute no member and terminalize every request as a no-dispatch
refusal. Builtin, dynamic capability and actor tests cover release, renewal,
reload, task-root retention, restore and source separation. Lease/batch event
append failures fence the actor before another model decision. The model tool
batch has a 32-call hard memory/queue bound; it is not a convergence constant.

Landed second behavior slice (still not a live performance claim):
`ToolDispatcher::execution_attribution` supplies bounded pre-dispatch purpose,
canonical resource targets and explicit verification-reuse policy. Runtime
joins targets with current task roots; dynamic capabilities fail closed,
shell/process remain Opaque, and output metadata cannot mint reusable
verification. Unrooted trusted path misses enter an eight-row,
workspace-revision-bound negative-fact table rather than the Obligation
Ledger. Equivalent reuse requires a live Workspace absence check plus a
successful `ExecutionNegativeFact::Reused` audit append; external appearance
or any admitted workspace mutation invalidates the fact. Current task roots
promote the next miss back to an obligation. Exact trusted verifier sources
are checkpointed under the task-anchor revision and PreferSurfaced when
verification is due, with semantic-role fallback if unavailable.
`negative_fact_*` eval counters, state/builtin/capability tests and an actor
test (two terminal read results, one real dispatch) cover the landed boundary.

Landed third behavior slice (still not a live performance claim):
`VerificationReuse::ExactCurrentWorld` requires a trusted SHA-256 host identity
digest for recipe/execution-profile/policy/environment inputs; raw environment
material is not stored. A successful
verification fact records exact tool, Runtime argument digest, task anchor,
user-directive revision and workspace revision. Runtime skips a later call
only if the whole tuple remains current and the `ExecutionVerificationPass`
reuse event appends; otherwise it dispatches. The no-dispatch result is
truthful (`executed=false`), remains a terminal action, and is split into
`verification_pass_recorded/reused` eval counters. New user directives and
any admitted workspace revision change force a real rerun. The state unit test
covers argument, environment, directive and workspace invalidation; the actor
test receives two terminal verification results from one dispatch.

Landed production entry point: bounded `verify.run { recipe_id }` recipes are
the only builtin process calls that can receive Verify attribution. Model argv
cannot shadow the host recipe; Core and the dispatcher are wired from one
recipe set. General project runners are `TaskScoped` and conservatively retain
Unknown mutation semantics. The first exact recipe is the generic
  manifest-free Rust test-target compile into `.focus-agent`; it binds a
  complete bounded workspace file snapshot, recipe revision, platform,
  resolved compiler and bounded complete environment. This covers transitive
  sibling modules; links/escapes, external-input directives, special files,
  overflow and pre/post identity drift downgrade to real execution. The real
runtime/tool deterministic bench now proves two requests settle from one
spawn (`Recorded=1`, `Reused=1`). Generic shell/process behavior is unchanged.

Open implementation must remain execution-only and staged:

- extend the landed exact-current completed-PASS identity with broader
  bounded coverage/obligation provenance and explicit host-declared
  equivalence classes; do not infer equivalence from commands. The API shape
  is designed 2026-08-26 in
  [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) (coverage domains with
  declaration revisions, class membership evaluated against current
  composition, fail-closed dispatch on every miss). Slice 1 landed 2026-08-26
  dormant — no shipped recipe declares a domain, so every request still
  dispatches: attribution carries bounded provenance, PASS facts store it,
  the state predicate and actor table check implement the widened reuse,
  and the reuse event plus eval metrics gained an exact/equivalent
  discriminator. Identical
  in-flight joining inside one batch is landed and proven: duplicate
  typed-verification calls settle as one spawn plus one truthful no-dispatch
  reuse (`ExecutionVerificationPass` Recorded/Reused, batch accounting
  `(2, 2, 1, 1)`), and the same joining holds for the negative-fact path;
  the sequential single-flight actor makes a second concurrent join window
  structurally impossible;
- extend the landed exact result-delivery/task/verification roots with trusted
  obligation-scoped provenance source tools;
- complete the table-driven crash/restart matrix. Normal, transient,
  recovery-refused, duplicate, oversized-batch, scope-open, admission and
  publication abort paths now settle or expose a missing terminal through the
  actor-local ledger; abrupt-loss replay evidence landed 2026-08-26:
  trace-only `analyze_batch_interruptions` in the agent-replay recovery
  report flags rounds killed between tool start and durable batch settlement
  with exact per-call counts, keeps live settle-time missing/unexpected
  terminals as a separate integrity signal, and ignores tool events outside
  any model round instead of inventing attributions;
- independently make accepted completion one-shot and terminal-safe.
  Landed 2026-08-26 as a deterministic proof matrix through the real actor,
  independent of the retained baseline (whose zero completion calls proved
  nothing): duplicate `task.complete` proposals inside one successful batch
  commit exactly one durable record (which concurrently-settling proposal
  wins the single slot is unspecified; uniqueness, no extra model round, and
  no fence are the contract), and an accepted completion stays terminal for
  its own turn while queued user input still drains into a clean follow-up
  turn with exactly one TaskCompleted, per-turn TurnCompleted events, and a
  completed-task catalog holding exactly the accepted record. The retained
  baseline still has zero completion calls, so its 49/65 versus 38/29 gap
  must not be attributed to this edge either way.

Do not lower the 18 KiB watermark globally, choose a call cutoff from this one
trace, parse arbitrary command strings to infer read-only/verification, or add
another generic "finish sooner" prompt. Before default behavior changes,
require deterministic exact/equivalent verifier reuse, stale settlement,
transient action, negative fact, cross-turn lease, discovery/reload and
already-satisfied-task tests, followed by at least two paired live repeats
with hidden success unchanged and no new p95/max-turn tail.

### PROTO-EVID-03 — Unknown suspends body reuse instead of deleting it (fixed 2026-08-23)

First live `ProtocolBodyCacheStats` accounting showed eligibility of
20–31 rows per longflow cell with hit rate exactly 0: every command
tool carries an Unknown footprint and each one physically cleared the
whole turn cache. Fix keeps correctness and reuses the existing
revalidation loop: Unknown mutations now *suspend* entries — bytes stay
in cache but are ineligible (**CachedBytesPresent ≠ BodyCurrentlyTrusted**);
BeforeModel hash revalidation restoring the same path@digest Fresh makes
the entry eligible again, a changed digest never passes the identity gate
and is left to LRU eviction. Known mutations keep physically dropping
their touched paths; counters split `invalidated` (physical) from
`suspended` (dormant). Deterministic regressions cover both branches.
Not a Context GC change: Context policy stays frozen.

### EVAL-IMMUTABLE-01 — live evidence attempts must not overwrite (fixed 2026-08-23)

A provider-503 retry overwrote good r1 artifacts; reconstruction had to
come from harness logs. Landed: `PairSink::claim` resolves the repeat
directory once per run — existing directories are never reused;
reruns land in `r{n}-attempt{k}` and failed attempts stay auditable.

## Freeze (not a defect)

### TOOL-GC-PHASE2 — surface pressure hysteresis (landed 2026-08-23)

Post-clock-fix long-flow (`longflow-post-clockfix-2026-08-23`) kept
re-loading optional builtins mid-task (13 loads, git.diff x4) and the
model even guessed `warm.<tool>` names, so the phase-2 gate was met.
`BuiltinToolDispatcher::gc` now cools only above a soft schema-bytes high
watermark, oldest-idle first, down to a low watermark (defaults
18_000/9_000; 0 restores pure idle semantics). The protocol evidence LRU
gate also fired; that cache landed 2026-08-23 (see PROTO-EVID-02).

### CTX-11 — Execution Coherence V1

**Status: Freeze Candidate** (MOD-OBS-01 / MOD-PROG-01 / turn
checkpointing landed 2026-08-21; the clean post-outage longflow pass
2026-08-23 held — Warm=Stored rereads stayed 0 and capability churn
stayed gone). Do not reimplement `ResumePoint`. A model-visible
`TaskProgress.task_changes` projection was tested and reverted 2026-08-24:
although one attribution turn shortened, its refined run amplified C to 127
rounds / 174 calls. Do not reintroduce it without the replay and paired-live
gate in `ROADMAP.md`. A generic current-workspace-authority standing prompt
also failed that gate in two repeats (C 64/79 and 72/76) and was reverted; do
not replace structured evidence with a "stop earlier" instruction.
Contract: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md).

## Open P2 — evaluation

### EVAL-01 — M15 live evidence is not yet auditable acceptance

Live cells must remain rebuildable from versioned per-cell artifacts
(manifest, events, verify, workspace hash). Partial `agent-eval.cell.v1`
bundles exist; executable hidden build/tests for a large suite do not.
Do not close M15 from smoke, `add_test`, or the 30-task pilot.

### EVAL-02 — layer the evals; do not reuse one A/B/C

Frozen `context-bench.v1` SPEC / pack digest stay frozen. Wave-1 live
(27 cells) is historical evidence under
`crates/agent-eval/evidence/context-bench-wave1/`. **Context** live
`context-mech.v2` (A/C × 3 tasks × 2 repeats = 12 cells) is under
`crates/agent-eval/evidence/context-mech/`. `add_test` is Tool Surface
(`historical_context=0`), not Context. Do not collect 300×3. Do not
retune GC from that live or from an `add_test` cell. Do not treat
`Likely optimization target` as a modification order.

### EVAL-03 — outcome split is partial; live attempts are non-decision-grade (fixed 2026-08-28)

Observed: dimensions are serialized separately, but overall PASS currently
requires only task completion + behavioral PASS + clean diff. It does not
require absence of runtime error, successful restore/continuation, or healthy
provider/runtime. Per-phase counters are committed only on a fully successful
driver return, restored and continued are conflated, and summary arithmetic is
not consistently rebuilt from per-cell facts. The exact verification recipe
also omits part of the mutable input set. EVAL-05 and EVAL-06 further contaminate
the retained attempts.

Risk: the two 2026-08-25 CompletionOpportunity attempts are useful diagnostics,
and one contains a real mechanism chain, but their pass ratios, medians and
arming rate cannot decide promotion. The candidate remains default-off.

Required fix and exit evidence: make every dimension typed PASS/FAIL/NOT_RUN;
require all mandatory dimensions, healthy provider/runtime and no runtime error
for overall PASS; classify provider/Runtime failures from typed sources rather
than error-string substrings; finalize phase counters even on failure; separate
restored, continued and completed; snapshot the complete bounded workspace
input set; require one non-empty identical opportunity key across
Offered -> Called -> Completed; and derive reports mechanically from per-cell
facts, including tool calls, prompt/schema tokens and tails. For `n=2`, report
both observations and any
midpoint used rather than naming the upper value as the median. Only then rerun
the frozen paired gate. Design and exit gate:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

Fixed 2026-08-28: the LT-RUN-05 WP4 evaluator truth reconstruction landed
(typed per-dimension records incl. healthy-provider and no-runtime-error
requirements, phase counters preserved on failure, restored/continued
separated, mechanically derived summaries, for-n=2 medians reported as both
observations), and the frozen paired gate then RERAN decision-grade — its
promotion verdict (FAIL; candidate ended) is recorded in
`evidence/opportunity-gate/REPORT.md` (2026-08-28 section).

### EVAL-07 — M15 v2 evidence projected the wrong contract (runtime fixed; evidence exit pending)

The three 2026-08-28 M15 attempts cannot be formal evidence. Missing
`task.complete` was turned into Runtime failure although M15 V1 made closure
report-only; diagnosis and migration manifests reused the retry-policy pack
id/digest; provider health came from message substrings; six
`max_output_tokens` outcomes were labeled transport failures; and aggregate
Markdown arithmetic drifted from cell facts. The raw bundles remain immutable
for forensic use, but their ratios, deltas and causal explanations cannot
select a serving, promote surface v5 or close M15.

The repaired `retry-pilot-cell-v3` path persists an acceptance profile,
PASS/FAIL/NOT_RUN verdict, actual pack identity/digest, typed failure class
and independent restored/exact-tuple/continued/turn/task facts. Provider
transport and harness failure produce NOT_RUN; model output limit produces
FAIL. The harness writes an exact window manifest and mechanically regenerates
the report while rejecting mixed identity, duplicate/missing cells, verdict
drift, absent terminal evidence, event gaps/loss, counter drift and paths
outside the evidence root. Evidence-write errors are fatal; the formal command
rejects a dirty tree, partial pack, repeat drift and protocol `auto`. Runtime
publishes typed failures and provider retry progress rather than making the
evaluator infer either from text. The deterministic suite is green
(`cargo test --workspace`, 2026-08-29) and the serving tuple stayed pinned
by the passing bounded one-cell preflights (both arms). The first formal
clean-tree 12-cell v3 window ran and was committed 2026-08-29 (window
`evidence/m15-window/_windows/1787966622822/`, mechanical report, 0 NOT_RUN):
11/12 PASS; the only failure is `retry_diag_dev` resume r2, which ends
without satisfying the hidden oracle on the overflow edge. M15 therefore
remains open — the frozen verdict demands all 12 cells pass, and the diag
overflow edge is the one recurring failure surface. Exit evidence for M15 is
still open: rerun one complete clean-tree v3 window and commit its
regenerated report with 12/12 PASS.

## Closed archive (index only)

Full text: git history of this file.

| ID | Closed as |
| --- | --- |
| 2026-08-10 repair pass | Workspace prefix, git.diff, focus/restore fences, context-service parity, journal/restore |
| CTX-01..CTX-10 | Episode, residency, fetch/search persist, store, Storage GC, GC ops, materializer, mid-turn signals, clocks, TaskAnchor |
| CTX-06..CTX-09 | GC/storage ops, materializer budget, working-set signals, lifecycle clocks |
| CORE-02..CORE-09 | Turn durability, checkpoint, output broker, System-role leak, cancel/process cleanup, TOCTOU opens, standing grants, schema budget |
| CORE-11 | HostToolPolicy registry, manifest → operator review → atomic admission/revocation, versioned snapshots and per-binding epoch fencing landed through 2026-08-26; M12 closure audit remains under CORE-01 |
| TOOL-01 | `search.grep` cancellation |
| TOOL-ENV-01, TOOL-EDIT-01, TOOL-VIEW-01, TOOL-ERROR-01 | Tool-quality preflight 2026-08-17 |
| MOD-AUTH-01 | `edit.patch files[]` multi-file authority widening → `EffectIntent::WorkspaceWriteSet` + all-paths `grant_matches` (2026-08-21; see PLATFORM_SECURITY.md) |
| MOD-AUTH-02 | Prepared effects report canonical `ActualWorkspaceWrite` (real path + real staged bytes); Core commit rejects `ActualExceedsApproved` outside the approved set (2026-08-21) |
| LONGTASK-01/02 | Catalog-cold progress CAS, actor safe-point resume install, coalesced checkpointing and same-task continuation landed deterministically (2026-08-24/25); remaining residuals are the LT-RUN-05 cold-resume matrix items under LONGTASK-03/04 (EXEC-REV-01 closed 2026-08-27) |
| CORE-11 | HostToolPolicy registry| CORE-12 | M13 sandbox gate: structured attestation with per-flag mechanism proofs, `required ⊆ actual` activation, native fail-closed UntrustedGenerated. Closed 2026-08-27 on the clean-tree closure-audit report (`evidence/platform-closure/m13/`) |
| Sandbox floor | `UntrustedGenerated.required` now includes `fs_read_confined` + `cpu_quota` (still fail-closed on native until provable); `process_spawn_controlled` → `process_count_quota` with a wire-compat serde alias (2026-08-21) |
| Foreground ack | `ContextConsumptionAck.foreground_item_ids` + engine counter: foreground bodies the model saw are observable (weak signal; no residency / admission change) (2026-08-21) |
| TOOL-02 | `search.grep` `path` accepts a file target (file-or-directory), removing a class of `path_not_found` tool failures (2026-08-21) |
| EVAL identity | Live evidence runs refuse a dirty workspace by default (`--allow-dirty` opt-in); the manifest records `source_tree_digest` over HEAD tree + tracked diff + untracked `crates/` sources (2026-08-21) |
| EVAL-04 | Source-identity self-pollution: a live run's own untracked evidence output made every cell after the first report `git_dirty=true` (the `context-mech-convergence` manifests record this). Identity scans now exclude `crates/agent-eval/evidence` — run outputs are not tested sources (2026-08-21) |
| CTX-12 | Not a code divergence: the parity tests had spawned a 9-day-stale `target/debug/agent-context-service.exe` (cargo test never refreshes that artifact; `serde(default)` hid the wire drift). Fixed with a test freshness guard that fails closed with a rebuild hint (2026-08-21). Scoped test runs need `cargo build -p agent-context-service` first. |
| PROV-01 | `provider-openai` loopback wire test failed through machine-wide proxies (Clash/V2Ray WinINET interception → gateway 502). Fixed with `OpenAiProvider::with_client` + a `no_proxy` test client (2026-08-21); production `new` keeps auto system proxy. |
| CONV-01 | Execution Evidence Frontier: ExecutionEvidence + FrontierDelta + ConvergenceState + `ExecutionFrontier` events + eval metrics; replay rebuild + conformance serde contracts (2026-08-23) |
| CONV-02 | Cross-tool convergence debt: FailureClass/FailureDomain split, RetryDomain::ExecutableResolution with host-trusted launch facts, no K-strikes decision recorded (2026-08-23) |
| PROTO-EVID-01 | Current-turn protocol body cache: ActiveTurn LRU, checkpoint+Fresh-gated rehydration, mutation invalidation; superseded by PROTO-EVID-02 correctness/observability fixes (2026-08-23) |
| PROMPT-AUTH-01 | Restored turn bodies moved from System-role focus frame to user-role context frame with regression test (2026-08-23) |
| EXEC-EVID-01 | Unified evidence currentness predicate shared by projection and sweep + restore bounds on convergence fields (2026-08-23) |
| PROTO-EVID-02 | Body cache source narrowed to fs.read exact bodies + per-round ProtocolBodyCacheStats event accounting in eval bundles (2026-08-23) |
| EVAL-IMMUTABLE-01 | Pair sink claims fresh repeat directories (`r{n}-attempt{k}`); existing evidence is never implicitly overwritten (2026-08-23) |

Do not start sourced `EpisodeOutcome`, GC retune, or a second ResumePoint
from this index.
