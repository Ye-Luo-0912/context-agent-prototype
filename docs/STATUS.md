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
| [`AUDIT_TODO.md`](AUDIT_TODO.md) | Confirmed defect queue |

## Now

- M10, M11, and M14 are closed at their named gates.
- Context V1 operational core and **Execution Coherence V1 are both
  freeze candidates**: the 2026-08-23 long-flow pass confirmed the
  coherence machinery (MOD-OBS-01 observation, MOD-PROG-01 stall,
  turn checkpointing) held; Warm=Stored rereads stayed 0. Do not retune
  or extend them as product work.
- **Execution Convergence V1 mechanism landed** (2026-08-23, all 22
  items checked — the checklist is now the historical record
  [`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md)):
  Evidence Frontier + FrontierDelta + `ExecutionFrontier` events and
  eval metrics; RetryDomain (`ExecutableResolution`, no K-strikes);
  per-turn protocol body cache with checkpoint-gated rehydration and
  event-level hit/miss accounting (`ProtocolBodyCacheStats`);
  versioned `HostPolicySnapshot`; unified surface pressure budget;
  replay frontier rebuild + conformance serde contracts. Verification:
  `agent-eval --convergence-bench` three deterministic scenarios PASS
  on the real runtime + real tool surface.
- Clean post-outage A/C longflow rerun completed 2026-08-23
  (n=2, all four arm-runs passed hidden verification): C r1 57 rounds,
  C r2 67 rounds (one rebuilt process-guessing chain), A r1 48 rounds,
  A r2 40 rounds. Facts in
  [`crates/agent-eval/evidence/longflow-post-convergence-2026-08-23/REPORT.md`](../crates/agent-eval/evidence/longflow-post-convergence-2026-08-23/REPORT.md).
  Residual convergence work is **obligation-scoped convergence**
  (CONV-03) plus the trust/observability fixes tracked in
  [`AUDIT_TODO.md`](AUDIT_TODO.md) and executed via
  [`TRUST_AND_OBLIGATION_TODO.md`](TRUST_AND_OBLIGATION_TODO.md):
  global frontier advances do not prove blocker progress; restored
  bodies must stay out of System role; dynamic capability producer
  metadata must not become trusted runtime facts.
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
  manifest → operator-review flow is still open (M12).
- Production always-load: `fs.list`, `fs.read`, `search.grep`,
  `artifact.read`, `edit.patch`, `task.complete`, `capability.manage`.
  Git / shell / write / `edit.replace` / `context.manage` are catalog-only
  except NeedEvidence PreferSurface of `context.manage`.
- Scripted `--compare-arm` still pins `fs.write` / `edit.replace` /
  `context.manage`. Do not change that pin.

**Do not claim M12, M13, or PLAT-06 closed.**

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

## P0 / P1

**P0 — trusted execution.** Finish M12/M13 gates in
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md): one brokerable
`EffectRequest` path; the landed `HostToolPolicyRegistry` now needs the
runtime admission flow itself — manifest → operator review → versioned,
mutable snapshot install (`HostPolicySnapshot{policy, revision, digest}`)
with the policy revision bound to operation authority, so an operator
update never re-interprets an in-flight operation. Revision enforcement
is **per binding** (`tool_name` + policy digest): adding `plugin.foo`
policy must not stale an already-approved `fs.write`; only replacing or
revoking the same binding affects that tool's later authority. Global
"revision changed → all old leases invalid" is rejected by design.
Do not build a second registry. Attestation is actual enforced
capabilities; generic process tools stay non-transactional.
UntrustedGenerated stays fail-closed on native. Multi-file
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
not more Context live cells.

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
  1200-character multi-file echo, and honest `NotApplied` / `Applied` /
  `Unknown` settlement.

Unit tests and clippy are green. The versioned `agent-eval.tool-edit.v2` pack
plus `agent-eval.tool-surface-edit.v3` gate also produced a source-bound r4
dirty-tree diagnostic pass over the current hardened implementation: 4
fixtures × 3 repeats, 12/12 raw-byte truth,
12/12 flow gate, 9/9 non-conflict first patch, 3/3 proactive stale routes,
zero patch refusal/fallback/confirm-read/recovery/unknown, and 42 rounds. Its
wall total was 164,417 ms and reported provider tokens were 258,325; it
preserved all r3 call-quality results while observing lower wall p50/p95. See
the r4 evidence `REPORT.md`. This proves the combined contract on that frozen
surface, not a general task-failure rate or a causal performance gain.

`TOOL-EDIT-02` remains open for the same frozen run on a clean source tree.
Deterministic external-race/crash/disk/journal fault coverage and staged-byte
accounting are the next breadth/reliability work; they are not evidence that
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

## Next milestone

Engineering mainline is **M12, then M13**, then a V1 candidate, then
formal M15. V2 Self-Iteration stays blocked.

Context evaluation: `context-mech.v2` 12-cell evidence exists; do not
expand to 27 or 300×3. Live `recall_after_fix` is refused.
`--compare-live-reasonable` is `add_test` only.
