# Context Bench CTX-11 live (3 tasks)

Not an M15 close and not a 27-cell rerun. Repeats=1. Run from clean
`bacf522` after eval-harness freeze, short-episode LLM compaction skip,
hot-reactivation counters (no policy change), `.focus-agent` git leak
reduction, and actor-owned `ResumePoint` / `TaskProgressView` (`CTX-11`).

- Date: 2026-08-18
- Binary git_head at run: `bacf522fafb550919e67890aa00b7bb5e860fe47`
  (`git_dirty=true` only because this evidence tree was untracked during
  the run)
- Schema: `agent-eval.context-bench.v1`
- `spec_sha256=2e8a89b27c5bf6ec7a2bdbc3011f3000c92585dc01c68517c277666df43314b4`
- `pack_digest=dfcfe75413ad4ee6e55dc1d34f6d17332d8f05ee5a334d82b14ef19fed2e15dd`
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Wall (sum of cells): 1781858 ms (~30 min)
- Cells: 7 written (`semantic_recall` A/C/B, `noise_recovery` A/C,
  `task_switch_long_b` A/C)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-bench-ctx11/<task>/r1`

Provider tokens = coding in/out + compactor in/out. Compactor totals come
from `ContextCompacted` events. Reactivation counters are instrumentation
only; they are not a policy change.

## Pass table

| task | A | C | B |
| --- | --- | --- | --- |
| semantic_recall | pass | pass | pass |
| noise_recovery | pass | pass | — |
| task_switch_long_b | pass | pass | — |

No A/C discord. Every cell recorded `run_completed`. Dynamic C compact
in/out is 0/0 on all three tasks (short-episode skip). Rolling B still
compacts.

## Provider cost

| task | A provider | C provider | C−A | C compact in/out |
| --- | ---: | ---: | --- | --- |
| semantic_recall | 359526 | 528050 | +168524 (+47%) | 0/0 |
| noise_recovery | 96159 | 68066 | −28093 (−29%) | 0/0 |
| task_switch_long_b | 246607 | 193027 | −53580 (−22%) | 0/0 |

`semantic_recall` rolling provider total: 952945 (compact 70488/4687), pass.

C `semantic_recall` cost is higher because of round count (C 52 vs A 33),
not compaction. Do not treat that as a compaction-policy miss.

## Resident bytes and reactivation (C only)

| task | C peak resident | C forgotten | auto-reactivation | selected/consumed | selected tokens |
| --- | ---: | ---: | ---: | ---: | ---: |
| semantic_recall | 9244 | 95 | 42 | 32/32 | 3794 |
| noise_recovery | 1427 | 14 | 6 | 1/1 | 12 |
| task_switch_long_b | 2927 | 36 | 14 | 4/4 | 105 |

Selected equals consumed on this run. Auto-reactivation still exceeds
selected (42→32, 6→1, 14→4); that gap is for a later ablation, not a
threshold retune in this slice.

## Next

Do not retune `active_threshold` / `archive_threshold` / `gc_max_generation`
or reactivation policy from this report. 27-cell wave is a separate ask.
Failed leftover dirs (`context-bench-ownership`, `ownership-retry`,
`pilot-30` SWE-bench cells) stay untracked.
