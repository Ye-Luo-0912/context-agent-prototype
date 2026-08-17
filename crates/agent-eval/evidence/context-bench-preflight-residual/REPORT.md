# Context Bench residual live (3 tasks)

Not an M15 close and not a 27-cell rerun. Repeats=1. Landed after
`TOOL-ERROR-01` residuals: failed tools do not heat C, evidence-gated
`MissingProjectMarker`, Core-owned `metadata._runtime`, marker refresh after
successful shell/process, and `ContextCompacted` cost accounting.

- Date: 2026-08-17
- Binary git_head at run: `2f0b0d77e5978aec2baeaddd2bb04454bcdade9d` (`git_dirty=true`; same residual as `09fb82d`)
- Schema: `agent-eval.context-bench.v1`
- `spec_sha256=12dc8e22f3a649b619f719f4a18e0cf73486a668aded4912ca93a469b22bc902`
- `pack_digest=00a6079ee601cd0004060acb168603c80d5d77dc62e77caf1782eccd88e2d38e`
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Cells: 7 written (`semantic_recall` A/C/B, `noise_recovery` A/C, `task_switch_long_b` A/C)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-bench-preflight-residual/<task>/r1`

Provider tokens = coding in/out + compactor in/out. Compactor totals come
from `ContextCompacted` events.

## Pass table

| task | A | C | B |
| --- | --- | --- | --- |
| semantic_recall | pass | pass | pass |
| noise_recovery | pass | pass | — |
| task_switch_long_b | pass | verify_failed | — |

Discordant A/C: `task_switch_long_b` (A pass, C miss `src/auth.rs` `rate_limit`).
Wave-1 had the opposite pair on that task.

## Provider cost

| task | A provider | C provider | C−A | C compact in/out |
| --- | ---: | ---: | --- | --- |
| semantic_recall | 437353 | 580221 | +142868 (+32%) | 38456/1051 |
| noise_recovery | 53004 | 120004 | +67000 (+126%) | 9483/335 |
| task_switch_long_b | 309979 | 294214 | −15765 (−5%) | 4752/195 |

`semantic_recall` rolling provider total: 418408 (compact 62924/2758), pass.

Wave-1 `semantic_recall` C compact was harvested as 0 while events showed
~34.6k input. This bundle records compact in `summary.json`.

## Next

n=1. Do not treat `Likely optimization target` as a modification order.
Do not implement ResumePoint from the `task_switch_long_b` label. Do not
expand to 27 cells from this slice alone.
