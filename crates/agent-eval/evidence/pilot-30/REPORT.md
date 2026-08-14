# EVAL-01.5 file-only calibration report

Non-acceptance pilot. `decision=pilot`. This is not the 300×3 gate and not
an M15 close. Amend n only by SPEC re-registration, never after seeing
acceptance cells. Scoring stays frozen.

- Date: 2026-08-14 (formulas rebuilt 2026-08-15: EVAL-01.3c)
- Sample: `agent-eval.pilot.v1` n=30 sha256 `fa8c5308520bc9b3b51cf0100bc14e78d2c2ca666d06010e27429455e0426431`
- Acceptance lock: `agent-eval.acceptance.v1` n=300 sha256 `7ff6b5ddefc7e6e6dc138e5e582de75b0cfc4f5eba831385cc550e4df8c124a7`
- SPEC: `agent-eval.analysis.v2` spec_sha256 `1448748962252fe90e16df55164f4b42259dab0dc8fda59616f56685c60f839d`
- Spend: 9 file-harvested tasks × 3 repeats × 3 engines = 81 cells
- SWE-bench: 21/30 tasks skipped (no clone/Docker opt-in)
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Rebuild: `agent-eval --pilot-calibrate crates/agent-eval/evidence/pilot-30`

## Coverage

```
coverage tasks=9/30 cells=81/270 missing=21 extra=0
ITT rates A=0.778 B=0.704 C=0.778
  size small n=6 A=0.667 (model 0.90) B=0.722 C=0.722
  size medium n=2 A=1.000 (model 0.70) B=0.667 C=0.833
  size large n=1 A=1.000 (model 0.40) B=0.667 C=1.000
diagnostic C-A: n_tasks=9 mean=0.000000 se=0.078567 LCL=-0.146100 (not a gate)
task-level corr(A,C)=0.783 (shared p predicts +)
repeat-level pooled phi(A,C)=0.357 (confounded by task difficulty; not A indep C | task)
task-residual corr(A,C)=-0.408 (A_i - task rate vs C_i - task rate; A indep C | task predicts ~0)
mean A var / Bernoulli p(1-p)=1.500 (1 ≈ iid repeats)
```

`analyze()` on the same cells: `decision=ineligible`
(`n_tasks=9 != 300` and evidence ids ≠ frozen acceptance set).
Outcomes: pass=61, verify_failed=20.

## Task-level ITT (A = append, C = dynamic)

| task | A | C | C−A |
| --- | --- | --- | --- |
| js-ms-minutes-shadow | 3/3 | 3/3 | 0 |
| js-ms-negative-parse | 3/3 | 3/3 | 0 |
| openai-wire-tool-names | 2/3 | 3/3 | +0.333 |
| python-itertools-batched | 2/3 | 3/3 | +0.333 |
| python-pep616-removeprefix | 3/3 | 2/3 | −0.333 |
| python-symbols-add-tests | 2/3 | 1/3 | −0.333 |
| rust-grep-cooperative-cancel | 3/3 | 3/3 | 0 |
| rust-jcs-canonical-objects | 3/3 | 3/3 | 0 |
| uuid-parity-keys | 0/3 | 0/3 | 0 |

## Cost (secondary)

Success stays ITT (all 27 intended A/C pairs, including usage-incomplete
failures). Token means now follow SPEC: a pair enters cost only when both
arms are `cost_eligible` (usage complete, seq contiguous, `broadcast_lagged=0`).

- Cost-missing: 6/27 pairs (rate 0.222). Unknown cost, not cost=0.
- Cost-eligible paired tokens, 21 pairs: mean A input 54318, C 56251, C−A +1933; rounds 8.0/7.9
- Both-pass, 18 pairs: mean A 53218, C 62479, C−A +9261; rounds 7.9/8.4

The previous “ITT tokens” line (27 pairs, A 46381 / C 48319) mixed six
ineligible pairs into the mean and is not a cost figure. Both-pass is
unchanged and still has selection bias (only cells where both arms passed).

## Notes versus the power model

- Task-level corr(A,C)=0.78 matches shared difficulty.
- Pooled φ(A,C)=0.36 is **not** a test of A ⟂ C | task; mixing easy and
  hard tasks inflates unconditional φ. The power-model diagnostic is
  task-residual corr(A,C)=−0.41 (n=9, noisy; do not retune n from this).
- Within-task A variance is 1.5× Bernoulli.
- Size labels do not match the power-model strata: small A=0.67 vs 0.90,
  driven largely by `uuid-parity-keys` 0/3 (ITT failures with complete
  usage still count as success=0; only usage-incomplete arms drop out of
  cost).
- Diagnostic C−A LCL=−0.146 would miss the −5 pp margin; that is not a
  gate result.

Do not collect 300×3 acceptance cells from this directory.

**EVAL-01.5.p1 (2026-08-15).** P0 host was kernel-fallback send 19904 + 12
rounds. That cannot host SWE-bench (empty context still overflowed; round
cap aborted tool loops). Remaining P0 SWE-bench spend is skipped. Do not
mix these P0 cells with a later P1 cohort in one ITT table. Do not re-run
file-only under P1 for this table. n/margin unchanged. Subsequent spends
use re-registered SPEC `2469ebcab44b6246ed35f1c82a35574d262ecb4b3912b858eb13dca407c344c2`.

The formal gate requires the exact 300 acceptance ids, not any ≥300
subset of the 509 pack.
