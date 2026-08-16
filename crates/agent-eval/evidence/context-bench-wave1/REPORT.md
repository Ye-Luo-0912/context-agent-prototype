# Context Bench wave-1 live (27 cells)

Not an M15 close and not a 300×3 ITT cell set. Repeats=1; no pre-planned
second repeat. Likely-optimization-target is attribution, not a code-change
order.

- Date: 2026-08-17
- Commit at run start: `89fbfbaba4549d484aa421590fae460df69644bd`
- Schema: `agent-eval.context-bench.v1`
- `spec_sha256=12dc8e22f3a649b619f719f4a18e0cf73486a668aded4912ca93a469b22bc902`
- `pack_digest=00a6079ee601cd0004060acb168603c80d5d77dc62e77caf1782eccd88e2d38e`
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Wall: 10238409 ms (~170 min)
- Cells: 27/27 written (12 A/C + rolling on `horizon_long`, `semantic_recall`, `task_switch`)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-bench-wave1/<task>/r1`

Provider tokens = coding in/out + compactor in/out.

## Pass table

`pass` / `verify_failed` / `timeout` (fixture turn timed out). Rolling only
where listed.

| task | A | C | B |
| --- | --- | --- | --- |
| horizon_short | pass | pass | — |
| horizon_long | timeout | timeout | timeout |
| long_refactor | timeout | timeout | — |
| long_refactor_relitigate | verify_failed | verify_failed | — |
| semantic_recall | pass | pass | pass |
| semantic_recall_fallback | pass | pass | — |
| supersession | timeout | timeout | — |
| supersession_leak | pass | timeout | — |
| task_switch | pass | pass | timeout |
| task_switch_long_b | verify_failed | pass | — |
| noise_recovery | pass | pass | — |
| noise_repeat_fail | pass | pass | — |

Solvable on at least one engine: `horizon_short`, `semantic_recall`,
`semantic_recall_fallback`, `supersession_leak` (A only), `task_switch`
(A/C), `task_switch_long_b` (C only), `noise_recovery`, `noise_repeat_fail`.

Timeouts (not C-vs-A policy): `horizon_long` A/B/C, `long_refactor` A/C,
`supersession` A/C, `supersession_leak` C, `task_switch` B. These are
anomalous cells; a second repeat is allowed later, not automatic.

Verify misses: `long_refactor_relitigate` A/C; `task_switch_long_b` A (C
passed). Discordant A/C: `supersession_leak`, `task_switch_long_b`.

Every why-report `Likely optimization target` on this wave was `none`.
Do not treat that line as a modification instruction.

## Provider cost (passed A/C pairs only)

| task | A provider | C provider | C−A |
| --- | ---: | ---: | --- |
| horizon_short | 40143 | 40730 | +587 (+1%) |
| semantic_recall | 460592 | 647269 | +186677 (+41%) |
| semantic_recall_fallback | 247306 | 251488 | +4182 (+1%) |
| task_switch | 141453 | 155400 | +13947 (+10%) |
| noise_recovery | 82093 | 109216 | +27123 (+33%) |
| noise_repeat_fail | 76730 | 79154 | +2424 (+3%) |

On this wave, passed long/semantic cells do not show a C token win versus A.
Resident bytes were usually lower on C when both passed (see per-cell
`summary.json`). Compactor tokens were 0 on the passed cells above.

## Next

Review traces by scenario. Second repeat only for A/C discordant, timeout, or
unexplained tasks. Context next cut still waits on that review:
Semantic Recall fail → Typed EpisodeOutcome; Task Switch fail →
Active/Suspended + ResumePoint; high runtime latency → GC/Catalog;
expensive compactor → compaction policy; no cost win on long tasks →
revisit Dynamic C.
