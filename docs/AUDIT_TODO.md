# Audit follow-up

Confirmed **open** defects only. Closed write-ups stay in git history of
this file; do not copy them back and do not reopen them as new work.

- Invariants: `AGENTS.md`
- Now/freeze/P0: [`STATUS.md`](STATUS.md)
- Execution: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
- Sandbox/M12: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md)
- Gates: [`ROADMAP.md`](ROADMAP.md)

M12/M13 must close before Self-Iteration. Do not add a database, vector
search, or learned ranking. Do not claim a milestone complete because
happy-path tests pass.

## Open P0 — trusted execution

### CORE-01 — M12/M13 residual (not closed)

First cuts landed: trusted `HostToolPolicy`, structured `EffectIntent`
(`ExecArgv` / `ShellExec`), `HostLifecycle` restart circuit,
`SandboxProfile` vs post-spawn `SandboxCapabilities`. External process
capabilities stay Disabled by default. Generic `shell.exec` /
`process.run` / `process.session` stay non-transactional (Core identity
before spawn, kill-then-reap, no rollback of child mutations).

Remaining OS isolation is the residual, not a new `MOD-18` slice:

- Linux UDP / raw / pathname-Unix
- Linux absolute OS-level reads
- Windows OS-level network
- I/O bandwidth quotas
- seccomp / AppContainer

`UntrustedGenerated` fails closed on native. WASI is V2. Matrix:
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md).

Do not make raw shell transactional. Do not close M12/M13 from the
first cut.

### CORE-10 — protocol remaining (not a transport swap)

`PLAT-00`–`PLAT-04` containment/protocol proof is landed. Remaining:

- PLAT-06 multiplexing (stay single-inflight in v0)
- PLAT-07 adapter envelope migration
- PLAT-08 Named Pipe/UDS (later)

Named pipes/UDS are not a fix for CORE-01. V1 still trusts Runtime in
the same address space.

### CORE-11 — HostToolPolicy registry & plugin admission (registry landed 2026-08-23)

Landed: `agent-contracts/src/host_policy.rs` is vocabulary only —
`HostToolPolicy`/`HostEffectBinding` carry owned names (serde-ready) plus
the `HostToolPolicies` lookup trait whose provided `effect_intent` is the
one derivation every consumer shares. The builtin table moved next to
its handlers in `tool-runtime` (`BuiltinToolPolicies`). Trusted
composition owns `agent-compose::HostToolPolicyRegistry`: builtins at
construction, operator-reviewed plugin bindings via `admit()`, which
refuses to shadow a builtin or duplicate an admission. One registry
instance is wired into the kernel lease path
(`CoreAuthorityConfig.host_policies`), the approval gate
(`TaskApprovalGate::with_host_policies`) and the capability dispatcher;
with no injection everything falls back to the declared-risk empty bound.

Still open (M12): the plugin manifest → operator review → `admit()` flow
itself. Until that lands, external write plugins stay safely
non-functional.

### CORE-12 — M13 attestation depth (open)

`SandboxCapabilities` booleans are the v1 floor. M13 acceptance should
upgrade to `SandboxAttestation { capabilities, backend,
backend_version, evidence }` so each enforced capability is
explainable (`fs_write_confined` → landlock ABI, `memory_quota` →
rlimit_as bytes). A boolean must not claim a stronger OS guarantee
than it delivers — `process_count_quota` was renamed from
`process_spawn_controlled` for exactly that (serde alias keeps the
wire compatible).

## Open P1 — Tool Surface reliability

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

### TOOL-CONTINUITY-01 — turn completion must not erase multi-turn task affinity (candidate; live success gate open 2026-08-24)

The one-shot completion trace isolated a lifecycle feedback loop rather than a
Context-size problem. Dynamic C called and committed `task.complete` on 9/15
turns versus A's 3/15. Each commit closed the task scope; the next user
directive started with a new task id and empty task-scoped `TaskProgress`, then
repeated capability discovery, list/read/search, and another completion.

The candidate separates implicit final-answer/turn completion from durable
task closure. `task.complete` is catalog-cold by default, leased for explicit
task-closure intent or an explicit Task requirement, and still discoverable
through `capability.manage`. An accepted clean completion terminates without a
confirmation model round; failed siblings and invalid verification gates keep
the recovery round. This changes no Context/GC threshold or retrieval score.

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
permanent. `LT-RUN-04` instead tests a body-free, once-per-verification-basis
`CompletionOpportunity` after positive terminal evidence, while retaining the
existing completion gate and explicit model call. It remains default-off until
an already-satisfied deterministic replay and at least two C off/on paired
normal/resume repeats pass the Roadmap item-8 success, median round/call and
tail gates. Closure discoverability and multi-turn non-premature closure must
both pass before this item closes.

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

### Fingerprint v2 — preview ≠ identity (fixed 2026-08-23)

The old `resolution_fingerprint` hashed only the 20-name cwd preview and
serialized `env` as an unordered map. Landed: scope_key =
digest(cwd identity + effective PATH + resolver rules version) is stable
across epochs; fingerprint additionally digests the full bounded
directory state (all entries sorted, 4096-entry/128 KiB caps, truncation
flag hashed) plus canonically sorted env pairs. Beyond-preview changes
move the epoch; HashMap iteration order cannot.

### TOOL-EDIT-02 — canonical edit first-attempt success (open)

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
  Core-managed prepare crash seams and conservative stage cleanup, but broader
  process/fault fixtures remain open. A successful edit currently performs the snapshot
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

The direct formal-acceptance blocker is a run of the same frozen pack on a
clean source tree; r4 deliberately used `--allow-dirty`, so all manifests say
`git_dirty=true` and `acceptance_eligible=false`. Acceptance measures
non-conflict first-patch success, correct proactive/reactive stale recovery,
edit-to-passing verification, failure class, fallback-to-shell/`fs.write`,
confirm reads, rounds, tokens, p50/p95 latency, bytes read/staged, commit
conflicts and partial recovery. Safety refusals may be a separate class, but
remain in end-to-end task success/time/cost. Add deterministic fault/race
fixtures before broader filesystem claims. M12 and M13 mainline does not move.

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

### EXEC-REV-01 — anchor CAS and verification basis are coupled inconsistently (open)

`ExecutionState` already carries `VerificationObligation.spec_revision`, but
verification pass/fail records currently copy the whole `anchor_revision`, and
every accepted anchor replacement calls `mark_spec_changed`. An out-of-turn
plan-only patch therefore marks otherwise Current verification stale. The
active `task.manage` path can then overwrite that task-resume state from the
turn execution after observing its own result at the new anchor revision, yet
exact-PASS reuse still rejects the older fact because its anchor revision no
longer matches. The same progress-only semantic change therefore has
path-dependent completion-freshness and reuse behavior. The code behavior is
confirmed; its contribution to live round/call cost has not yet been measured.

Retain `anchor_revision` as the whole-record compare-and-swap fence, but give
verification basis an independent revision meaning. Progress-only changes
advance CAS without invalidating verification; goal, constraint and
authoritative-criterion changes advance both. Treat `current_interpretation`
through an explicit host policy. ActiveTurn, task resume, completion and exact
reuse must share one currentness predicate; cover both revisions through
checkpoint/restore.

### LONGTASK-03 — safe-point artifact is not a complete cold-restorable checkpoint (open)

The actor safe-point snapshot writes an empty capability list and persists that
actor-owned value directly; only an external `RuntimeInstance::checkpoint()`
merges the host registry. `CheckpointStore` has a write-plus-rename path but no
matching artifact load API, whole-artifact checksum, file sync or parent-
directory sync. The file is atomically visible but has no power-loss durability
claim. The live pilot has capability-aware mode disabled, so this is a
production recovery-contract defect, not the cause of its missing closure.

Make one actor/host ownership handshake produce a complete checkpoint at the
safe point, persist and load/validate it with explicit platform durability
semantics, and acknowledge the exact durable resume revision. Prove artifact
restore with a fresh Runtime and Context engine, without phase-one in-memory
state. Contract and tests:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

### LONGTASK-04 — checkpoint failure does not fail-closed continuation (open)

`await_pending_checkpoint` records failure and returns no error. The
continuation path waits for it, does not test the failed outcome, and then
starts the next turn. `CheckpointDurable` also lacks the revision that its
contract says it acknowledges. A failed safe-point write can therefore leave
debt visible while continuation still emits `TaskContinuationStarted`.

Track `resume_state_revision`, `required_durable_revision` and
`durable_revision`. Capture, write, checksum, rename or acknowledgement
failure must make `continue_active_task` return an error before any model
request or continuation event. Retry of the same required revision releases
the fence only after its durable acknowledgement.

### CONV-01 / CONV-02 / PROTO-EVID-01 — closed 2026-08-23

All three landed in Execution Convergence V1 (see
[`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md) and
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)); write-ups moved to
the second-round section below and to the closed archive. The remaining,
narrower residuals are CAP-OBS-01 and CONV-03 there.

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
[`TOOL_RESULT_ENVELOPE.md`](TOOL_RESULT_ENVELOPE.md). Still open before
Self-Iteration: introduce the typed host-trusted `ToolExecutionFacts`
channel so context heating, ExecutionState, Evidence Frontier,
RetryDomain and Verification consume runtime/verified facts instead of
producer metadata at all (capability default = empty facts; effect
receipts and workspace handles generate Runtime-owned facts).

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

### CONV-04 — execution attribution + capability leases (partial; attribution/negative-fact slices landed 2026-08-24)

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

- extend the landed exact-current completed-PASS identity with broader bounded
  coverage/obligation provenance, identical in-flight joining, and explicit
  host-declared equivalence classes; do not infer equivalence from commands;
- extend the landed exact result-delivery/task/verification roots with trusted
  obligation-scoped provenance source tools;
- complete the table-driven crash/restart matrix. Normal, transient,
  recovery-refused, duplicate, oversized-batch, scope-open, admission and
  publication abort paths now settle or expose a missing terminal through the
  actor-local ledger; abrupt process loss still needs replay evidence;
- independently make accepted completion one-shot and terminal-safe; the
  retained baseline has zero completion calls, so this cannot be presented as
  the cause of its 49/65 versus 38/29 gap.

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

### EVAL-03 — pilot conflates closure with behavioral acceptance (open)

The `retry_policy_dev` deterministic gate is green and the first four C live
cells ran. All four canonical cells lack `TaskCompleted`, but the evaluator
returns the lifecycle error before executing its post-run diff/cargo checks.
Thus their reports cannot distinguish a behaviorally correct workspace with
missing closure from an incorrect implementation. Additional provider-dead
attempts must stay outside the canonical four-cell behavior aggregate.

Record behavioral oracle, allowed diff, task closure, continuation and
provider/runtime health independently; keep the final PASS conjunctive. If the
workspace is inspectable, missing closure or a late provider failure must not
suppress read-only acceptance checks. Then use the final evaluator in the
default-off CompletionOpportunity C off/on promotion gate; only a promoted,
frozen setting may enter same-model A/C. The pilot is development evidence,
not M15 acceptance. Design and exit gate:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

### EVAL-05 — resume twin does not prove a cold artifact restore (open)

Both phases receive the same `ContextEngine` object. After the durable event,
the harness separately captures an in-memory full checkpoint and passes that
value directly to phase two instead of loading the artifact named by the
event. `SimpleContextEngine::restore` replaces its state, so this does not
prove an in-memory leak; it proves only that cold-engine/cold-artifact recovery
has not been tested.

Phase two must allocate a fresh Context engine and host, retain only the
artifact locator/digest/revision across the boundary, load that artifact, and
prove the same task/directive/authority identity with no duplicated effect.

### EVAL-06 — live cargo acceptance runs agent-editable tests (open)

The frozen seed contains no harness-owned tests. The post-run `cargo test`
therefore executes tests the evaluated agent may add inside its editable
workspace; the other hidden predicates are implementation-marker diagnostics.
That cargo result is a useful self-check, not an independent behavioral oracle.

Add a network-free harness-owned external test crate or equivalent isolated
oracle for retry bounds, transient/permanent behavior, saturation/overflow and
public API compatibility. Multiple correct implementations must pass without
a golden patch; keep the workspace self-check as a separately named outcome.

## Closed archive (index only)

Full text: git history of this file.

| ID | Closed as |
| --- | --- |
| 2026-08-10 repair pass | Workspace prefix, git.diff, focus/restore fences, context-service parity, journal/restore |
| CTX-01..CTX-10 | Episode, residency, fetch/search persist, store, Storage GC, GC ops, materializer, mid-turn signals, clocks, TaskAnchor |
| CTX-06..CTX-09 | GC/storage ops, materializer budget, working-set signals, lifecycle clocks |
| CORE-02..CORE-09 | Turn durability, checkpoint, output broker, System-role leak, cancel/process cleanup, TOCTOU opens, standing grants, schema budget |
| TOOL-01 | `search.grep` cancellation |
| TOOL-ENV-01, TOOL-EDIT-01, TOOL-VIEW-01, TOOL-ERROR-01 | Tool-quality preflight 2026-08-17 |
| MOD-AUTH-01 | `edit.patch files[]` multi-file authority widening → `EffectIntent::WorkspaceWriteSet` + all-paths `grant_matches` (2026-08-21; see PLATFORM_SECURITY.md) |
| MOD-AUTH-02 | Prepared effects report canonical `ActualWorkspaceWrite` (real path + real staged bytes); Core commit rejects `ActualExceedsApproved` outside the approved set (2026-08-21) |
| LONGTASK-01/02 | Catalog-cold progress CAS, actor safe-point resume install, coalesced checkpointing and same-task continuation landed deterministically (2026-08-24/25); residuals are LONGTASK-03/04 and EXEC-REV-01 |
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
