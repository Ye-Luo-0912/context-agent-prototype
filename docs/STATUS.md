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
- Context V1 operational core is a **freeze candidate**; Execution
  Coherence V1 is **RC** (its blockers — MOD-OBS-01 observation,
  MOD-PROG-01 progress/stall, turn checkpointing — landed 2026-08-21;
  freeze re-designation waits for the next live evidence pass). Code
  still runs; do not retune or extend them as product work.
- M12 first cut: structured `EffectIntent` + trusted `HostToolPolicy`,
  multi-file `WorkspaceWriteSet` bounds, and commit-time
  Actual ⊆ Approved (`MOD-AUTH-01`/`02`).
- M13 first cut: `SandboxProfile` vs post-spawn `SandboxCapabilities`.
- PLAT-06 slices 1–2 (lifecycle / cancel-ACK) landed. Multiplexing is
  not in v0.
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
`EffectRequest` path; a `HostToolPolicyRegistry` so plugin admission
can install operator-reviewed bindings (external write plugins fail
closed today — safely non-functional); attestation is actual enforced
capabilities; generic process tools stay non-transactional.
UntrustedGenerated stays fail-closed on native. Multi-file
`EffectIntent` and commit-time Actual ⊆ Approved (`MOD-AUTH-01`/`02`)
landed 2026-08-21 — do not reopen them without new authority evidence.

**P1 — Context live evidence, not Context retune.** `context-mech.v2`
12-cell A/C live ran 2026-08-21; facts in
[`crates/agent-eval/evidence/context-mech/REPORT.md`](../crates/agent-eval/evidence/context-mech/REPORT.md).
Do not retune GC from it. `add_test` is Tool Surface
(`historical_context=0`), not Context. Engine packs foreground first
(actual tokens). GC-induced reread is `Warm` + `Stored` only.

**P1 — open-turn convergence evidence.** Execution Convergence first
cut landed 2026-08-21: MOD-OBS-01 (a refused mutation is still an
observation), MOD-PROG-01 (stall advisory + deterministic duplicate
refusal), turn checkpointing (`TURN_FRAME_KEEP_EXCHANGES`). The open
P1 is the live tool-loop / convergence failure the 12-cell run
exposed; next evidence is production-surface late-semantic op5
reproduction aimed at Tool/Execution Convergence — not 12 → 24 → 48
context cells.

## Next milestone

Engineering mainline is **M12, then M13**, then a V1 candidate, then
formal M15. V2 Self-Iteration stays blocked.

Context evaluation: `context-mech.v2` 12-cell evidence exists; do not
expand to 27 or 300×3. Live `recall_after_fix` is refused.
`--compare-live-reasonable` is `add_test` only.
