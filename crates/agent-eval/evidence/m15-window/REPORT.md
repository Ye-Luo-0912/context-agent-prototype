# M15 formal acceptance window — 2026-08-28 (development plane) — window FAILED; M15 stays open

Scope: the frozen `M15_ACCEPTANCE.md` development pack — three tasks
(`retry_diag_dev`, `retry_migrate_dev`, `retry_policy_dev`) × {normal,
resume} × 2 repeats = 12 cells, C engine on the pinned serving
(`gpt-5.6-luna` @ PinAI), advisory switch OFF everywhere, immutable bundles
under `m15-<task>-<mode>/r{1,2}/`. The banked planes (platform closures,
context-mech.v2, edit-gate v4, convergence bench) are cited per the design
and were not rerun.

## Per-cell facts (mechanically derived from `dimensions.json`)

| cell | behavior | diff | closure | continuation | provider | PASS |
| --- | --- | --- | --- | --- | --- | --- |
| diag normal r1 | fail | pass | failed | n/a | healthy | no |
| diag normal r2 | fail | pass | failed | n/a | healthy | no |
| diag resume r1 | pass | pass | failed | restored | healthy | no |
| diag resume r2 | fail | pass | failed | restored | healthy | no |
| migrate normal r1 | pass | pass | failed | n/a | healthy | no |
| migrate normal r2 | pass | pass | failed | n/a | healthy | no |
| migrate resume r1 | pass | pass | failed | restored | healthy | no |
| migrate resume r2 | pass | pass | failed | restored | healthy | no |
| retry normal r1 | pass | pass | completed | n/a | healthy | **yes** |
| retry normal r2 | pass | pass | failed | n/a | healthy | no |
| retry resume r1 | pass | pass | failed | restored | healthy | no |
| retry resume r2 | pass | pass | failed | restored | healthy | no |

Summary: 1/12 cells PASS. Zero provider stalls, zero runtime failures, all
resume cells restored and continued on cold artifacts — the harness and
truth chain held for the whole window.

## Gate application (per `M15_ACCEPTANCE.md` §4)

Plane PASS requires every cell PASS. The development plane scored 1/12, so
**the window FAILED and M15 is not closed**. The other planes remain banked
evidence; per the design, no plane borrows another's cells.

## What the facts separate

- The runtime plane is clean: 12/12 provider-healthy, 12/12 no stalls, all
  resume cells restored and continued, no duplicated effects, every
  non-passing cell failed on honest model outcomes (lifecycle closure) or
  the behavioral oracle — not on infrastructure.
- Behavior separated from lifecycle: `retry_migrate_dev` scored behavior
  pass in 4/4 cells (the model performed the migration correctly) yet 0/4
  closed the task lifecycle; `retry_policy_dev` scored behavior pass 3/4
  with 1 completed. The recurring blocker across all ten failing-but-
  behavioral-pass cells is lifecycle closure (`TaskCompleted` never
  reached), the same affordance gap the ended CompletionOpportunity
  candidate measured.
- `retry_diag_dev` is the hardest task: behavior pass 1/4 — the diagnosis
  contract (report + minimal fix + corrected test) was not met by this
  serving within its rounds.

## Consequences

- M15 remains **not closed**; the development plane must pass a future
  frozen window of the same design.
- The recorded blocker is specific and measurable: lifecycle closure
  affordance under model autonomy. Per the opportunity-gate decision, that
  candidate is ended; closing this gap is a product-design question for a
  future frozen proposal, not a rerun of the same window.
- Behavior/diff infrastructure, the oracle suite, and the cold-resume chain
  are demonstrated decision-grade: they held across 12 live cells with zero
  harness-attributed failures.
