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
- `fs.write` remains a catalog-only blind whole-file upsert for scripted-arm
  compatibility; a future compatible schema needs explicit create vs
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

### CONV-03 — obligation-scoped convergence (mechanism landed 2026-08-23, residual narrowed by live evidence)

Global frontier advance does not prove blocker resolution (C r2's
13-attempt guessing loop kept peak=4 < advisory 5 via interleaved
advances). Landed: typed `ExecutionObligation` ledger keyed by domain +
precondition fingerprint (`resolution_fingerprint` = cwd listing + PATH
+ env overrides stamped host-trusted on process NotFound); unrelated
progress can never resolve an obligation; resolution requires
precondition change or same-domain success; ≤2 bounded UNRESOLVED
BLOCKER warning lines render beside the global advisory. Evidence
argument identity now uses the Runtime-computed ArgumentDigest.
Pending: longflow/bench evidence under the new ROADMAP gate, and the
LaunchResolutionFact hard-refusal note that its revision guard is
deliberately conservative until fingerprints are recomputable
pre-dispatch without I/O.

Live narrowing (`../crates/agent-eval/evidence/longflow-post-obligation-2026-08-23/REPORT.md`,
2026-08-23): guessing chains rebuilt in *both* C repeats. Fingerprints
are stable within a chain, but attempts never escalate because (a) any
successful command resolves all ExecutableResolution obligations — a
successful `rustc` build clears the unrelated "compiled tests exe not
found" blocker — and (b) successful builds change the cwd listing, so
the next failure carries a new fingerprint and supersedes the old row.
Open work: resolution must require a precondition-matched success (same
fingerprint), not domain-any-success; and obligation warnings are
TASK PROGRESS-only, so bundles cannot yet prove they fired.

### PROTO-EVID-03 — body cache starves under Unknown-footprint command pressure (observed 2026-08-23, no retune)

First live `ProtocolBodyCacheStats` accounting: eligibility is high
(20–31 offered rows per longflow cell) but hit rate is exactly 0. Every
`process.run` / `shell.exec` has an Unknown mutation footprint and each
one invalidates the whole turn cache (invalidated 6–16 per cell), so
command-dense trajectories starve the cache the checkpoint-missing
motive was measured on. The all-entries invalidation is deliberately
conservative and stays frozen; this entry records the measured fact so
any future policy change must bring its own evidence and design, not a
threshold tweak.

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
stayed gone). Do not reimplement `ResumePoint`.
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
