# Current state

Snapshot: **2026-09-05**. Machine source of truth:
[`docs/state.json`](state.json) — the current-status text in `README.md` and
this file must agree with it. Full decision record: [`STATUS.md`](STATUS.md).
Formal verdicts live only in the immutable
`crates/agent-eval/evidence/m15-window/_windows/<id>/REPORT.md` reports.

## Basis

- Head `33081b4` — M15 closure (`d004836`) plus the docs-state tranche and
  CI-load-tolerant turn-integration test deadlines. Runtime code is
  unchanged from `050aa8e`. CI is green on the head (`33898496334`); the
  earlier `d004836` run had failed its Windows full suite on test wait
  deadlines under CI load (closure conditions are unaffected — they pinned
  the candidate source `050aa8e`/`a882789`, which stayed green).
- `main` has no branch protection yet.

## M15 — closed at the frozen gate

- M15 closed **2026-09-04** (commit `d004836`): the tenth predeclared
  12-cell v4 window returned the mechanical verdict **PASS 12/12, 0 NOT_RUN**
  (`_windows/1788539149184`, run log `window10.log`): diag 4/4, migrate 4/4
  and policy 4/4 — the two stochastic surfaces green in one window for the
  first time — with three completed closures (report-only dimension).
- Ten earlier v4 runs (eight valid FAILs, one censored, one authorized
  rerun) stay immutable diagnostics; the best pre-closure run was the ninth
  window's 11/12 (`_windows/1788533384841`).
- Next ordered gates per [`ROADMAP.md`](ROADMAP.md): `LT-EVAL-06` evaluation
  breadth and the packaged V1 release gate. V2 Self-Iteration is unblocked
  by M15 but remains a separately proposed and accepted phase.

## Product stage

- **Pre-alpha.** Phase 2 (Reliable Local Agent alpha) started 2026-09-04.
  Landed: checked model configuration (`32ea622`); unified manual/automatic
  checkpoints on the Runtime `CheckpointStore` envelope, with retention
  scoped to real store artifacts and a fail-closed legacy decoder
  (`CHECKPOINT-PRODUCT-01`, 2026-09-05, uncommitted at write time).
- Next product queue (order per [`ROADMAP.md`](ROADMAP.md#route-to-a-usable-local-agent)
  and the 2026-09-05 code review, [`reviews/2026-09-05-code-review.md`](reviews/2026-09-05-code-review.md)):
  a real kill/restart acceptance matrix; strict `ProviderProfile`/
  `ProductProfile` with digests and an explicit `SamplingPolicy`; a
  Runtime-owned status snapshot; proof refresh as a cancellable operation;
  a bounded TUI transcript; packaging and clean-machine doctor.
- Default context is dynamic in-process. `context-service`,
  completion-opportunity, settlement-projection and recovery-surface remain
  experimental.

## Document organization (2026-09-05)

Done in this pass: `docs/state.json` (machine-readable single source),
this file, drift fixes in `README.md` / `STATUS.md` /
`LONG_TASK_EVALUATION.md`, and `CONTEXT_RUNTIME_TODO.md` moved to
[`archive/`](archive/) (historical design queue, never live contract).

Still pending (target structure agreed 2026-09-05): CI document-consistency
gate (README/STATUS vs `state.json`, window counts vs `_windows/` manifests,
doc links, tool inventory vs generated manifest, Cargo `rust-version` vs CI
toolchain); split `EXECUTION_MODEL.md`, `PRODUCT_ALPHA.md`,
`RECOVERY_RUNBOOK.md`, `CONFIGURATION.md`, `COMPATIBILITY.md`,
`CONTEXT_FRAME_V1.md` out of the oversized combined docs; move closed
audit/history sections to `archive/`; protect `main` with the existing
cross-platform CI as required check.
