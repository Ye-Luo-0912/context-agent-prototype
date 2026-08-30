# M15 v4 window failure diagnosis (2026-08-30)

Post-window evidence analysis of the first formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788093162603`), following
the M15_ACCEPTANCE §5 rule 4 return-to-diagnosis from its valid FAIL (9/12).
All facts below are read from the immutable per-cell event streams and the
harness `verify.json` records; no new live run produced them.

Verdict shape: 9/12 pass, 0 NOT_RUN, all cells provider healthy. The three
failures fall into two mechanisms: two `retry_diag_dev` cells fail the same
saturation boundary, and one `retry_policy_dev` cell exhausts the 48-round
tool budget before closing.

## 1. retry_diag_dev: `checked_shl` truncates overflow to zero (2 cells)

Failed cells: `retry_diag_dev-normal/r2-attempt7`, `retry_diag_dev-resume/
r1-attempt7`. Both pass agent-authored self-checks (`workspace_self_check=pass`)
and the `rust.workspace` verification, then fail the harness behavioral oracle
`cargo test --test m15_diag_oracle` and the frozen marker check
"shift corrected and overflow-safe".

The model's final `src/backoff.rs` (verbatim from `verify.json` of
`retry_diag_dev-normal/r2-attempt7`):

```rust
pub fn next_delay(attempt: u32, config: &RetryConfig) -> u64 {
    let exponent = attempt.saturating_sub(1);
    if exponent >= 64 {
        return if config.base_delay_ms == 0 { 0 } else { config.max_delay_ms };
    }

    let raw = config.base_delay_ms.checked_shl(exponent).unwrap_or(u64::MAX);
    raw.min(config.max_delay_ms)
}
```

Oracle failure evidence (`verify.json` commands entry):

```
assertion `left == right` failed
  left: 0
 right: 1000
at tests/m15_diag_oracle.rs:19:5   // assert_eq!(next_delay(63, &cfg(100, 1_000)), 1_000)
```

Mechanism. Rust `u64::checked_shl(rhs)` only checks the shift count
(`rhs >= 64` returns `None`); it neither checks nor prevents value overflow.
For `attempt = 63` the exponent is 62, so `100u64 << 62` needs 69 bits: the
seven bits of `100` are shifted entirely past bit 62 and truncated, and the
shifted value is exactly `0`. `raw.min(max_delay_ms)` then returns `0`
instead of saturating at `1_000`. The large-attempt "saturation" therefore
wraps to zero — precisely the seeded defect the directive forbids
("saturate at the cap rather than wrapping to zero").

The golden/harness-frozen solution widens to `u128` before the shift
(`(base_delay_ms as u128) << shift`, then `min` and `as u64`), which keeps
the arithmetical value until the cap clamp. The marker check requires
`u128` or `leading_zeros` — it is not a style constraint: it names the
implementation class that cannot truncate overflow. The model's
`checked_shl` + `unwrap_or(u64::MAX)` is semantically unsafe for values
whose doubled expansion overflows, so both the marker and the oracle reject
it on the same defect.

Same-pack variance is real: `retry_diag_dev-normal/r1-attempt7` and
`-resume/r2-attempt7` passed, so the model alternates between a correct
`u128`-widening solution and the invalid `checked_shl` variant across cells.
This is the same failure class as the three v3 windows (`checked_shl`
saturation strategy), now with byte-level evidence.

## 2. retry_policy_dev: no closure loop after a refused completion (1 cell)

Failed cell: `retry_policy_dev-normal/r2-attempt7`,
`error_class=round_budget`, "phase one failed: tool round budget exhausted
after 48 rounds". Behavior marker checks are 3/6 (transient classification,
retry-loop bounds and saturation markers absent from the final tree); the
agent's own `rust.workspace`/`jobrunner.exact` verifications keep passing.

Proposal refusal (verbatim warning):

```
completion proposal refused: invalid request: completion is not ready:
trusted verification is not current; 1 acceptance criterion/criteria lack
current coverage; 1 unresolved failed command(s) remain
```

The refusal fires because the last model mutation happened on the current
revision without the exact verification succeeding on that exact world, and
an unrelated failed command (a `shell.exec`/`process.run` failure earlier in
the trace) was never resolved.

After the refusal the model re-runs `jobrunner.exact` and `rust.workspace`
(both pass — the criterion would now be covered), but it never resolves the
failed command and never submits `task.complete` again. The remaining rounds
spend on `git.diff`/`git.status` pairs, `process.run`, `fs.read` and
`capability.manage` churn until the 48-round budget is exhausted.

Mechanism. The cell fails on closure rhythm, not on capability routing:
- the proposal is submitted while the verification basis is stale and a
  failed command is unresolved, so the typed gate correctly refuses;
- the post-refusal recovery loop is incomplete: it refreshes the PASS but
  neither binds the failed command nor re-proposes completion;
- the trailing loop consumes the budget without converging.

The resume cells of the same pack close cleanly (7+24 and 10+31 rounds); the
normal r1 of the same pack closes in 11 rounds, so a sufficient solution
exists and was reached in two of the four policy cells.

## 3. What the window does NOT show

- No harness, transport, context or verification-tuple defect: 0 NOT_RUN,
  all provider healthy, `replay_complete=true` everywhere, mechanical report
  regenerated from the persisted dirs with identical identities.
- No marker/oracle mismatch: the two mechanisms above are real model
  behavior faults (overflow-truncation saturation; incomplete closure loop)
  caught by the frozen oracles. Changing the fixture, oracle or marker would
  require an explicit acceptance refreeze and is not proposed here.
- No preflight/serving signal: the single-cell preflight passed on
  `retry_policy_dev` normal, but the window's second policy normal cell
  exhausted the budget — one cell cannot predict the 12-cell spread.

## Routes for the next candidate decision (evaluation-config, not engine work)

- serving/model: the §0 serving pin may be reopened only after a bounded
  representative preflight proves a new tuple; the gate itself is unchanged.
- round budget: 48 rounds is a frozen window parameter; changing it for a
  candidate is an evaluation-config change that voids the failed window
  (which stays immutable) and requires re-preflight, not a harness fix.
- No Context/GC/retrieval/packing change is authorized by this diagnosis.

Verdict: diagnosis complete for the three failures; the candidate selection
is a user decision.