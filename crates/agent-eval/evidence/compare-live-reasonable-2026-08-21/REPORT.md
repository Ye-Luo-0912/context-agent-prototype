# Production-surface live compare (`add_test`)

Not an M15 close. Repeats=1. Same fixture and model as the 2026-08-14
`--compare-live-reasonable` smoke. Cells ran dirty on `36f50be` plus the
uncommitted sandbox / Execution Coherence tree that this report lands.

- Date: 2026-08-21
- Command: `agent-eval --compare-live-reasonable`
- Evidence: `crates/agent-eval/evidence/compare-live-reasonable-2026-08-21/`
- Fixture: `add_test` only (item 28). Arm order rolling → dynamic →
  append
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in
  this tree)
- Binary git_head at run: `36f50bedcc912119375c939605bc3725c9d9ab42`
  (`git_dirty=true`)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/compare-live-reasonable-2026-08-21/add_test/r1`

Live coding compare now reuses production
`ToolLifecycleConfig::default()` (item 23). Write / edit /
`context.manage` are catalog-only on that default; the model must
`capability.manage search` then `load` before `edit.patch`. Scripted
`--compare-arm` still pins `fs.write` / `edit.replace` /
`context.manage`. Frozen Context Bench SPEC is untouched.

## Pass table

Hidden verify **3/3**, C−A = 0. One user turn each. Round counts are
inner model rounds, not user turns.

| arm | hidden | wall | model_in | model_out | provider_total | rounds | tools |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| append (A) | pass | 46.5s | 61475 | 859 | 62334 | 10 | 9 |
| rolling (B) | pass | 54.2s | 68225 | 900 | 78707 | 11 | 11 |
| dynamic (C) | pass | 53.2s | 62063 | 898 | 67735 | 10 | 11 |

Each arm: `capability.manage` ×4 then `edit.patch` ×1. Each arm had one
failed `process.run` (`process_exit`). C: `forgotten=11`,
`lifecycle=18`, `recovery_failed=11`. A lifecycle = 0.

## Why rounds jumped versus 2026-08-14

The 08-14 live harness still **pinned** write/edit, so the first model
round could patch. This run is production catalog-only. Typical C
trajectory:

1. Discover (`fs.read` / `fs.list` / `search.grep`; C packed these in
   round 1)
2. `capability.manage search` with a query that returned 0 hits
3. Catalog `search` (no query) listing `edit.patch` as available
4. `load edit.patch`
5. `edit.patch`
6. Re-read / `load process.run` / failed `process.run` / `task.complete`

A/B follow the same load path. Round inflation is a **tool-surface**
effect, not a C working-set split. `historical_context_tokens` stayed 0
on every round of this 1-turn fixture.

08-14 same command / model / `add_test` (write/edit still pinned):

| wave | A in / rounds / tools | C in / rounds / tools |
| --- | --- | --- |
| first all-pass | 17498 / 1t / 2 | 24057 / 1t / 5 |
| 503 retry | 17516 / 3r / 2 | 33321 / 5r / 5 |
| **this run** | **61475 / 10r / 9** | **62063 / 10r / 11** |

## Per-round tokens (C did not beat A)

`prompt_tool_schema_tokens` in the cell summary is the **sum across
rounds**, not the last-round schema. First-round schema is 866; after
`edit.patch` load it is 1301; after `process.run` load it is 1502.

Provider `model_used.input_tokens` this run:

| arm | n | mean | min | max |
| --- | ---: | ---: | ---: | ---: |
| append | 10 | 6148 | 5462 | 6750 |
| rolling | 11 | 6202 | 5462 | 6738 |
| dynamic | 10 | 6206 | 5462 | 6791 |

Versus 08-14 retry C (33321 / 5 ≈ **6664** / round), this C mean is
≈ **6206** (−7%). Versus the 08-14 first wave if that 24057 was one
packed call, first-round size dropped because write/edit schemas are no
longer always-on (today round 1 is 5462). Versus **this run's A**, C
per-round did **not** drop (6206 vs 6148). Cell totals rose because
more rounds still each cost ~6k (schema + accumulating turn frame).

Do not read live totals as a materialization-cost estimand. That remains
`--compare-arm`. This n=1 does not close M15.
