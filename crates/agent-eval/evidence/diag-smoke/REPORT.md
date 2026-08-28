# retry_diag_dev calibrate-pack live smoke — 2026-08-29

## Identity

- Clean HEAD: `1f2df4d` (`Add --diag-smoke live runner for the calibrated
  retry_diag_dev pack`), preceding fixture calibration commit `f232fb7`.
- Serving: PinAI `/v1` + `gpt-5.6-luna`, protocol `auto` (negotiates
  Responses), context window default (128,000), key present.
- Command: `agent-eval --diag-smoke` (both modes, one repeat each, recovery
  switch off). This is a fixture-solvability smoke, not a formal M15 cell and
  not a paired gate.

## Cells

| cell | verdict | rounds | tools | failed outputs | closure | hidden oracle |
| --- | --- | --- | --- | --- | --- | --- |
| `retry_diag_dev` normal | FAIL | 8 | 15 | 0 | active | 5/6 asserts pass, target exits non-zero |
| `retry_diag_dev` resume | FAIL | 5+4 | 20 | 0 | completed | 5/6 asserts pass, target exits non-zero |

Both provider-healthy, zero NOT_RUN rows, wall ~83–93 s each.

## What the model wrote

Both cells produced the same fix shape in `src/backoff.rs`:

```rust
let shift = attempt.saturating_sub(1);
let raw = config
    .base_delay_ms
    .checked_shl(shift)
    .unwrap_or(config.max_delay_ms); // resize cell uses u64::MAX instead
raw.min(config.max_delay_ms)
```

The off-by-one is fixed correctly, the wrong seed test table is replaced, and
`DIAGNOSIS.md` names `next_delay` and the direct 1-based shift. The failure is
exactly the calibrated overflow edge: `checked_shl` returns `None` only when
the shift amount is ≥ 64, not when bits shift out of the value
(`100u64.checked_shl(62)` returns `Some(0)`). The oracle's
`next_delay(63, cfg(100, 1_000)) == 1_000` therefore gets `0`, the target
fails, and the strengthened hidden check (`saturating_sub(1)` **and**
`u128`/`leading_zeros`, no `attempt.min(63)`) rejects the fix. The resume cell
called `task.complete` while asserting the fix "preserves saturation
behavior".

## What this proves

- Under the **old** check table, both of these fixes would have passed every
  hidden needle (`saturating_sub(1)` present, `attempt.min(63)` absent) while
  failing the oracle — the exact evaluator-validity hole the 2026-08-28 audit
  described. The calibration closes it: needle, oracle and golden now agree.
- The deterministic self-check (`cargo test -p agent-eval m15_pack`) still
  proves the u128-widened reference solution passes the oracle and all
  needles, so the calibrated pack is solvable.
- The pinned serving consistently finds the visible defect but not the
  overflow-safety edge, naming `next_delay` and the mechanism correctly in
  `DIAGNOSIS.md`. This is now valid, interpretable evidence about the serving
  on the diag fixture, not a harness artifact.

## Decision

Keep the calibrated fixture as the M15 diag pack. Diag cells measure
overflow-safe saturation difficulty; a failing diag cell in the formal window
is an honest reported fact (closure is report-only). No fixture, oracle or
serving change follows from this smoke. Remaining readiness work is unchanged:
Completion Convergence V1 readiness, then the exact-source one-cell preflight,
then the formal 12-cell window.

## 2026-08-29 one-cell product preflight (normal arm PASS)

Added after the two-cell failure above, once Completion Convergence V1
readiness landed: the pre-window checklist's remaining item was a bounded
one-cell product preflight on the same pinned serving, proving the calibrated
fixture is solvable before the formal window spends its 12-cell budget.

- Clean HEAD: `09cce69` (the `--conv-tail` commit), same frozen fixture
  digest `2fff5157…eeb`; no fixture, oracle or serving change.
- Command: `agent-eval --diag-smoke normal` (one repeat, recovery switch
  off). Evidence is under `retry_diag_dev-normal/r1-attempt2/dynamic/`.
- Verdict: PASS — `retry_diag_dev` normal in 14 rounds / 22 tool calls /
  1 failed output / 139,886 ms; zero provider retries, contiguous event
  stream, 6 durable checkpoints, hidden oracle green (`cargo test --test
  m15_diag_oracle` 3/3, exit 0) and replay complete.
- Settlement exposed: first `SettledCandidate` frontier event at seq 153
  (carrying a pending `verification_changed` debt); pre 9 rounds / 15 calls,
  post 5 rounds / 7 calls. The model closed with an ordinary final message —
  `closure=active`, no `task.complete`, no auto-close.
- The single failed tool output is one `edit.patch` stale-revision retry
  (6 edit attempts, 5 committed changes, 1 failure on the first patch of the
  final rewrite).
- Diagnosis markers: 5/6 assertions pass; the only miss is the `backoff.rs`
  "shift corrected and overflow-safe" needle, because the written solution
  guards `exponent >= u64::BITS` and uses `checked_mul` + `min(max_delay_ms)`
  instead of the reference `u128`/`leading_zeros` shape. The oracle and the
  seeded unit tests are green, so this is a needle-shape miss, not a
  functional failure.

Interpretation: this is one positive cell on the frozen calibrated fixture.
It clears the one-cell preflight item (the pinned serving tuple can start,
edit, verify and settle the diag fixture) and shows the earlier two-cell
failure was the model not solving the overflow edge, not a harness artifact.
It is not a formal M15 cell, makes no efficiency claim, and does not close
M15; a resume-arm one-cell preflight was not part of this bounded run.