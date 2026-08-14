# EVAL-01.5 file-only calibration report

Non-acceptance pilot. `decision=pilot`. This is not the 300×3 gate and not
an M15 close. Amend n only by SPEC re-registration, never after seeing
acceptance cells. Scoring stays frozen.

- Date: 2026-08-14
- Sample: `agent-eval.pilot.v1` n=30 sha256 `fa8c5308520bc9b3b51cf0100bc14e78d2c2ca666d06010e27429455e0426431`
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
repeat-level phi(A,C)=0.357 (A indep C | task predicts ~0)
mean A var / Bernoulli p(1-p)=1.500 (1 ≈ iid repeats)
```

`analyze()` on the same cells: `decision=ineligible` (`n_tasks=9 < 300`).
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

- ITT tokens, 27 pairs: mean A input 46381, C 48319, C−A +1937; rounds 7.0/7.0
- Both-pass, 18 pairs: mean A 53218, C 62479, C−A +9261; rounds 7.9/8.4

## Notes versus the power model

- Task-level corr(A,C)=0.78 matches shared difficulty.
- Repeat-level φ(A,C)=0.36 is above the A ⟂ C | task near-zero prediction; n=9 is small.
- Within-task A variance is 1.5× Bernoulli; several cells finished in 1–3s with `model_in=0` and were ITT failures, not dropped cells.
- Size labels do not match the power-model strata: small A=0.67 vs 0.90, driven largely by `uuid-parity-keys` 0/3.
- Diagnostic C−A LCL=−0.146 would miss the −5 pp margin; that is not a gate result.

Do not collect 300×3 acceptance cells from this directory. The remaining
21 SWE-bench sample ids still need `--include-swebench` plus clone/Docker.
