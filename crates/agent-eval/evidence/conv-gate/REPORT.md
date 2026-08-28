# Completion Convergence V1 paired live gate — 2026-08-29

**Decision: the settlement boundary mechanism and its exposure accounting
are verified, but the convergence-efficiency criterion is NOT satisfied.
Do not claim Completion Convergence closed, and do not claim M15 closed.**

## Identity

- command: `agent-eval --conv-gate` (normal + resume, 2 paired repeats)
- cells: 4 (1 pack × 2 modes × 2 repeats); all `retry-pilot-cell-v3`
- pack: `retry_policy_dev` (frozen spec digest; the 55-round/129-call
  bounded-readiness blocker from the recovery-surface audit)
- source: clean tree at `deed96c`; `git_dirty=false`
- serving: `gpt-5.6-luna` @ `https://api.pinaic.com/v1`, `protocol=auto`
  (Response negotiation), context window 128,000
- candidate switch: completion opportunity off, recovery surface off; the
  derived settlement boundary is runtime-owned and always on
- provider health: 4/4 `healthy`; zero `NOT_RUN`, zero transport cells

## Results

| cell | verdict | behavior | diff | closure | continuation | total rounds | total calls | settled at | post rounds | post calls |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| `retry_policy_dev` normal r1 | PASS | pass | pass | completed | n/a | 13 | 35 | round 11 | 2 | 7 |
| `retry_policy_dev` normal r2 | PASS | pass | pass | completed | n/a | 25 | 56 | round 19 | 6 | 21 |
| `retry_policy_dev` resume r1 | PASS | pass | pass | completed | restored_and_continued | 40 | 89 | round 11 | 29 | 58 |
| `retry_policy_dev` resume r2 | PASS | pass | pass | completed | restored_and_continued | 25 | 57 | round 22 | 3 | 5 |

Settlement exposure: 4/4 cells observed at least one
`SettledCandidate` `ExecutionFrontier` event (`settlement_seen=true`
recorded in each `summary.json`); no cell is inconclusive on exposure.
All cells reached `task.complete` → durable `TaskCompleted`
(`closure=completed`), and none auto-closed: the task stayed `Active`
across the settled moment and only the model's explicit completion
proposal committed the durable record.

## Mechanically reconstructed facts

Each cell's `summary.json` carries the event-derived settlement fields
(`settlement_seen`, `settlement_pre_rounds`, `settlement_pre_calls`,
`settlement_post_rounds`, `settlement_post_calls`), reconstructed from the
`ExecutionFrontier` event stream; the first `SettledCandidate` label in the
stream sets the pre/post split and later same-label events do not move it.
The leading invariant of the audit (zero exposure ⇒ inconclusive, never a
pass) is enforced by the runner: any cell without exposure would be
reported as inconclusive.

## Frozen-criteria evaluation (per CONV-CLOSE-01)

1. **Mandatory behavior/diff/resume parity**: satisfied — 4/4 behavior
   pass, 4/4 allowed-diff pass, 2/2 resume cells restored_and_continued.
2. **No lost unfinished work**: no evidence of truncation; resume cells
   continued the exact directive with the exact restore tuple
   (`exact_resume_tuple_matched=true`, `restored=true`, `continued=true`).
3. **Lower rounds/calls after the first settled candidate**: NOT satisfied
   for the resume arm. Normal post-settlement median is 6 rounds / 21
   calls; resume post-settlement median is 29 rounds / 58 calls (resume r1:
   29 rounds / 58 calls after the round-11 settled candidate).
4. **No new max tail**: not satisfiable to claim — this gate had no
   A/C comparison arm; it measured tail behavior against the settled
   boundary only.
5. **Outcome-free actions and repeated cleanup remain counted**: yes —
   failed outputs are in the denominator (resume r1 had 9 failed outputs:
   `edit.patch` 5, `process.run` 3, `shell.exec` 1; rereads 22), and the
   runner prints them per cell.

## Interpretation

The mechanism goal of the slice is met deterministically and now also
under a real serving: a derived settled candidate appears exactly when the
trusted verification basis covers the current world and the obligation
ledger is empty; the model keeps the choice (every cell chose durable
closure, none was forced or auto-closed); cancel/resume and cold restore
re-derive the same boundary from the persisted resume.

The efficiency goal is not met: the resume arm still spends a long
post-settlement tail (29/58 in resume r1) on edit/process/verify retries
with repeated reads — the same family as the original 55-round/129-call
tail. In resume r1 the model had already reached a settled candidate by the
restore boundary and kept working on diagnostic-marker misses
(`transient classification`, `retry loop bounds`, `delay growth`
markers missed in `src/error.rs`/`src/lib.rs`), which are real remaining
work rather than pure cleanup — the boundary was observed, but it did not
curtail the repair loop. Do not retune the boundary from this run: the
derived state machine is frozen; the open question is policy/surface
pressure on the model side, not the settlement state.

## Next step

A paired A/C comparison at the same serving with the same pack and at
least two repeats, where the only variable is the settlement-derived
surface/projection treatment, is required before claiming lower
post-settlement tail. Until then, M15 stays open and Completion
Convergence V1 remains at "mechanism verified, efficiency not claimed".