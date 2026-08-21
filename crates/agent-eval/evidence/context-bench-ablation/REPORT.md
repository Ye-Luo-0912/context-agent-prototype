# Context Bench C-ablation (semantic_recall only)

Not an M15 close, not a 27-cell rerun, and **not** a causal proof of
ResumePoint or episode-card value. Frozen Context Bench `SPEC` /
`FROZEN_SPEC_SHA256` / `FROZEN_PACK_DIGEST` were not changed.

- Date: 2026-08-18
- Command: `agent-eval --context-bench-ablation` (default repeats=2)
- Binary git_head at run: `6ae40772ccad0831eeb78f3078651bd8111a585f`
  (`git_dirty=true`: uncommitted contract/ablation work plus this evidence)
- Schema: `agent-eval.context-bench.v1` pair extra `ablation=semantic_recall_c_only`
- `spec_sha256=2e8a89b27c5bf6ec7a2bdbc3011f3000c92585dc01c68517c277666df43314b4`
- `pack_digest=dfcfe75413ad4ee6e55dc1d34f6d17332d8f05ee5a334d82b14ef19fed2e15dd`
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Wall (sum of 6 cells): 1180164 ms (~20 min)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-bench-ablation/semantic_recall/rN`

Arms (C-only, shuffled per repeat; both repeats landed identity order):

| arm | compaction | TaskProgress in Focus |
| --- | --- | --- |
| `current` | adaptive (semantic-delta skip) | projected |
| `force-compact` | always pay for an LLM episode card | projected |
| `no-progress` | adaptive | omitted |

## This wave does not close CTX-11

Five of six cells died on the provider (`HTTP 400 Upstream request failed`
or a stream decode error) before hidden verify. Pass/fail and round-count
deltas are **not** an arm effect. Do not retune reactivation thresholds.
Do not treat `task_switch_long_b` or this table as ResumePoint V1 value.

| repeat | current | force-compact | no-progress |
| --- | --- | --- | --- |
| r1 | error (HTTP 400, turn 2) | error (HTTP 400, turn 7) | error (HTTP 400, turn 6) |
| r2 | error (HTTP 400, turn 6) | **pass** | error (stream decode, turn 3) |

Hidden `src/protocol.rs` contains `Hello` failed on every truncated cell.
Only `force-compact` r2 finished the trajectory (`asserts=2/2`).

## What the knobs did measure

Prompt-layer sums are across `ModelStarted` (approx tokens, not provider
billing). `no-progress` kept `prompt_task_progress_tokens=0` on both
repeats. `current` r1 is also 0 because the run died at turn 2 before
ResumePoint filled. `current` r2 = 336; `force-compact` r2 = 3010 — that
gap is mostly round count (17 vs 45), not a per-round Progress tax.

| cell | rounds | provider total | compact in/out | progress | history | events | unique | selected/consumed |
| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | --- |
| r1 current | 8 | 52514 | 0/0 | 0 | 224 | 0 | 0 | 0/0 |
| r1 force-compact | 22 | 177124 | 19386/444 | 1098 | 11014 | 15 | 11 | 8/8 |
| r1 no-progress | 19 | 127426 | 0/0 | 0 | 5508 | 8 | 8 | 4/4 |
| r2 current | 17 | 138947 | 19015/663 | 336 | 6597 | 9 | 9 | 4/3 |
| r2 force-compact | 45 | 426724 | 37779/1244 | 3010 | 41659 | 40 | 35 | 28/28 |
| r2 no-progress | 11 | 69479 | 0/0 | 0 | 2804 | 4 | 4 | 4/4 |

`force-compact` paid the LLM episode card on both repeats. Adaptive arms
can still compact later (`current` r2 19015/663) when an episode has a
semantic delta or focus generation ≥ 4. That is not a skip-policy miss.

Reactivation **events** and **unique ids** are different counters. Example:
`force-compact` r2 is 40 events / 35 unique / 28 selected. Do not report
that as a single utilization rate (and do not compare it to the older
42→32 line in the CTX-11 three-task report). Selected ≈ consumed on these
cells. FileObservation selected/consumed stayed 0; ToolObservation carried
the selected set.

## Next

A clean 6-cell needs a provider that does not 400 mid-trajectory. Until
then, keep CTX-11 **NARROWED**, not closed. Do not mix this directory into
the frozen wave-1 pack (`context-bench-ctx11`).
