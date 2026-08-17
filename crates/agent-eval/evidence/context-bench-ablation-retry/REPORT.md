# Context Bench C-ablation retry (semantic_recall only)

Not an M15 close and not a 27-cell rerun. Frozen Context Bench `SPEC` /
`FROZEN_SPEC_SHA256` / `FROZEN_PACK_DIGEST` were not changed.

The first wave under `evidence/context-bench-ablation/` died on gateway
HTTP 400 `Upstream request failed` (5/6 truncated). This retry ran after
classifying that wrapped 400 as retryable; genuine 400 (illegal tool name,
context overflow) is still not retried.

- Date: 2026-08-18
- Command: `agent-eval --evidence-dir crates/agent-eval/evidence/context-bench-ablation-retry --context-bench-ablation`
- Binary git_head at run: `6ae40772ccad0831eeb78f3078651bd8111a585f`
  (`git_dirty=true`: uncommitted contract/ablation work plus this evidence)
- Schema: `agent-eval.context-bench.v1` pair extra `ablation=semantic_recall_c_only`
- `spec_sha256=2e8a89b27c5bf6ec7a2bdbc3011f3000c92585dc01c68517c277666df43314b4`
- `pack_digest=dfcfe75413ad4ee6e55dc1d34f6d17332d8f05ee5a334d82b14ef19fed2e15dd`
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in this tree)
- Wall (sum of 6 cells): 2380918 ms (~40 min)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-bench-ablation-retry/semantic_recall/rN`

| arm | compaction | TaskProgress in Focus |
| --- | --- | --- |
| `current` | adaptive (semantic-delta skip) | projected |
| `force-compact` | always pay for an LLM episode card | projected |
| `no-progress` | adaptive | omitted |

## Pass table

All six cells finished the trajectory. Hidden `file_content+command` 2/2.
`usage_incomplete=false`. Every cell recorded `run_completed`.

| repeat | current | force-compact | no-progress |
| --- | --- | --- | --- |
| r1 | pass (49 rounds) | pass (37 rounds) | pass (47 rounds) |
| r2 | pass (45 rounds) | pass (47 rounds) | pass (58 rounds) |

Omitting TaskProgress, or forcing episode cards, did **not** fail this
one task. That is not a token-savings proof and not an M15 non-inferiority
result: live arms do not share a tool trace.

## What the knobs measured

Prompt-layer sums are across `ModelStarted` (approx tokens). Provider
total = coding in/out + compact in/out.

| cell | rounds | provider | compact in/out | progress | history | events | unique | selected/consumed | reread |
| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | --- | ---: |
| r1 current | 49 | 469130 | 0/0 | 4451 | 63515 | 56 | 45 | 36/36 | 10 |
| r1 force-compact | 37 | 359838 | 38139/1127 | 2891 | 30306 | 37 | 33 | 21/21 | 10 |
| r1 no-progress | 47 | 486851 | 0/0 | 0 | 59776 | 56 | 44 | 29/29 | 12 |
| r2 current | 45 | 397978 | 0/0 | 3633 | 39956 | 46 | 38 | 26/26 | 10 |
| r2 force-compact | 47 | 513558 | 38338/1312 | 12826 | 47153 | 45 | 37 | 22/22 | 10 |
| r2 no-progress | 58 | 558369 | 0/0 | 0 | 46655 | 48 | 32 | 22/22 | 16 |

**TaskProgress projection.** `no-progress` is 0 both repeats. `current`
is 4451 / 3633. Per round that is ~91 and ~81. Progress is small next to
historical context (7–9% of that layer on `current`). Do not claim
ResumePoint is why the task passed.

**Adaptive episode skip.** `current` and `no-progress` compact 0/0 on
both repeats. `force-compact` paid ~38k/1.2k both times. On this
long-protocol trajectory the skip is doing what it says; forcing cards
adds a real compact bill. Whether that bill reduces coding tokens is
**mixed** (r1 force-compact cheaper because 37 vs 49 rounds; r2
force-compact more expensive at similar rounds). That is a trace
difference, not a policy delta.

**Reactivation counters.** Events, unique ids, and selected are different
denominators. Example: r1 `current` is 56 events / 45 unique / 36
selected. Do not report that as one utilization rate. Selected equals
consumed on every cell. FileObservation selected stayed 0; ToolObservation
carried the selected set. Do not retune `active_threshold` /
`archive_threshold` / `gc_max_generation` from these numbers.

## What this still is not

- Not ResumePoint V1 closure. Checkpoint restore, suspended-root
  downgrade, and named hard-caps remain on CTX-11.
- Not a proof that TaskProgress changes success. All three arms passed.
- Not a 27-cell wave and not an ITT gate.
- Keep `semantic_recall.v1` as a long-protocol trajectory. The constraint
  lives on TaskAnchor; this task does not prove GC-forget-and-recall.
