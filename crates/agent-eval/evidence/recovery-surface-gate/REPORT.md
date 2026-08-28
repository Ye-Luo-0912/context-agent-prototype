# Recovery-surface paired gate — 2026-08-28

**Decision: the recovery surface candidate does NOT promote. Keep the
catalog-cold baseline (`fs.mkdir` absent from the default surface; the
typed-recovery host switch stays off).**

## Identity

- command: `agent-eval --recovery-surface-gate` (normal + resume, 2 repeats)
- cells: 24 (3 packs × 2 modes × 2 repeats × 2 arms); all `retry-pilot-cell-v3`
- source: clean tree at `1a239479db4e8365a2cbe8b0f2e043d46a4b823d`;
  `git_dirty=false`, `source_tree_digest` recorded per cell
- serving: `gpt-5.6-luna` @ `https://api.pinaic.com/v1`, `protocol=auto`,
  context window 128,000
- only variable between arms: `recovery_surface` (off = catalog-cold
  baseline; on = typed missing-parent refusal surfaces exactly `fs.mkdir`
  with `RecoverySurface` provenance for one decision). The setting is
  recorded per cell in `dimensions.json`.
- provider health: 24/24 `healthy`; zero `NOT_RUN`, zero transport cells.

## Results

| pack | mode | off pass | on pass | off median rounds | on median rounds | off median calls | on median calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `retry_diag_dev` | normal | 0/2 | 0/2 | 10 | 10 | 19 | 18 |
| `retry_diag_dev` | resume | 0/2 | 0/2 | 12 | 11 | 27 | 28 |
| `retry_migrate_dev` | normal | 2/2 | 2/2 | 14 | 17 | 28 | 35 |
| `retry_migrate_dev` | resume | 2/2 | 2/2 | 19 | 13 | 33 | 29 |
| `retry_policy_dev` | normal | 2/2 | 2/2 | 31 | 48 | 75 | 107 |
| `retry_policy_dev` | resume | 2/2 | 1/2 | 30 | 55 | 72 | 129 |

Totals: off 12/12 pass, on 11/12 pass. Max total rounds: off 31, on 55.

One `on` arm cell failed with `round_budget` (tool round budget exhausted
after 48 phase-two rounds; 55 total rounds, 129 tool calls, 7 failed
outputs). All other cells passed on behavior and allowed-diff.

## Frozen-criteria evaluation

1. **Equal mandatory success**: not satisfied. off 12/12 vs on 11/12.
2. **Lower median aggregate rounds**: not satisfied. `retry_policy_dev`
   normal +55% and resume +83%; `retry_migrate_dev` normal +21% (resume
   −32% in the other direction).
3. **Lower median aggregate calls**: not satisfied (same cells, same
   direction; on-arm calls ≥ off-arm calls in 5 of 6 pack/mode cells).
4. **No new max/p95 tail**: not satisfied. on-arm max 55 rounds vs off-arm
   max 31 (+77%).
5. **Failed outputs remain counted**: yes, reported above; on-arm had more
   failed outputs in the failing cell (7).

Conclusion: the recovery source costs more decisions than its per-round
schema cost, so it fails the promotion contract and the always-ready compact
schema becomes the fallback candidate (unexercised; would require its own
paired comparison before promotion).

## Why the candidate showed no gain

`fs.mkdir` is catalog-discoverable, and the model already loads it through
`capability.manage` in the baseline: `fs.mkdir` was called once with success
in 6 of 8 `retry_policy_dev` cells (both arms) and the typed missing-parent
refusal was not the limiting failure in these cells (triggers were
`edit.patch`). The one-decision surface therefore did not remove a measured
bottleneck.

## Independent finding: `retry_diag_dev` fails 0/8 in both arms

The hidden oracle (`m15_diag_oracle`) failed in all eight diagnosis cells
regardless of arm. The failure is consistent: the model corrects the visible
1-based-shift defect, writes `DIAGNOSIS.md`, and removes the seed's wrong
table assert — all file-level needle predicates pass — but the edited
implementation is still
`attempt.saturating_sub(1).min(63)` then `base << shift`, which wraps to
`0` for attempts ≥ 64 (`u64` shift-out). The oracle's
`growth_doubles_then_saturates` then sees `left: 0, right: 1000`.

The pack's saturation edge is therefore a genuine contract requirement the
serving misses on every attempt; the needles reward the visible fix without
catching the overflow. This is a calibration finding for the formal M15
window (which would fail its four `retry_diag_dev` cells as-is), not a
recovery-surface result — both arms are identical on this dimension.

## Evidence

Per-cell `dimensions.json` records the arm, verdict, rounds, tool calls,
wall time, failed outputs, provider health and typed error class;
`verify.json` records the oracle replay and workspace snapshots; full event
streams are in each cell's `events.jsonl`.