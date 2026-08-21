# context-mech.v2 live (12 cells)

Not an M15 close. Not a GC retune. Frozen `context-bench.v1` SPEC is
untouched. `add_test` is Tool Surface, not this pack.

- Date: 2026-08-21
- Command: `agent-eval --context-mech-run` (default repeats=2)
- Evidence: `crates/agent-eval/evidence/context-mech/`
- Schema: `agent-eval.context-mech.v2`
- `spec_sha256=d3ee79ce7ec8c51bea6976a8a6e0df77e30b116930f1032f809a67f42c93e198`
- Cells: A/C × 3 tasks × 2 repeats = **12**
- Tasks: `late_semantic_constraint`, `resume_operational_state`,
  `no_semantic_episode` (no rolling)
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in
  this tree)
- Binary `git_head` at run: `ea5d4485d6b5b6b1b0172119664266a460d6f03a`
  (`git_dirty=true`)
- Wall: ~33 min
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/context-mech/<task>/r<n>`

Live cells reuse production `ToolLifecycleConfig::default()`. Scripted
`--compare-arm` and mech ops still pin `fs.write` / `edit.replace` /
`context.manage`. Arm order is Fisher-Yates of `[append,rolling,dynamic]`
with rolling dropped.

## Pass table

Hidden file+command checks passed on **all 12 cells**. ITT `outcome`
is pass on 11/12; C `late_semantic_constraint` r2 is `error` (48-round
cap on the last user op). Hidden C−A = 0. Do not read total `model_in`
as a same-trace A/B: traces diverge.

| task | r | A hidden | A in / rounds / tools | C hidden | C in / rounds / tools | C ITT |
| --- | ---: | --- | --- | --- | --- | --- |
| late_semantic_constraint | 1 | pass | 253592 / 28 / 29 | pass | 268452 / 33 / 33 | passed |
| late_semantic_constraint | 2 | pass | 239031 / 27 / 28 | pass | 852129 / 60 / 70 | **error** (round cap) |
| resume_operational_state | 1 | pass | 135329 / 19 / 27 | pass | 128787 / 20 / 31 | passed |
| resume_operational_state | 2 | pass | 124969 / 18 / 24 | pass | 221823 / 32 / 42 | passed |
| no_semantic_episode | 1 | pass | 79620 / 12 / 9 | pass | 108775 / 16 / 15 | passed |
| no_semantic_episode | 2 | pass | 116558 / 16 / 15 | pass | 106506 / 16 / 20 | passed |

`no_semantic_episode` r1 C has `provider_tokens_lower_bound=true`.
`late_semantic_constraint` r2 C does too (error cell). Usage otherwise
complete; seq contiguous; lag 0.

## Mechanism notes (not retunes)

**Late semantic constraint.** r1 C searched the catalog (`search=1/4`)
and recovered 2 forgotten items; hidden Hello + ping + `wire_v1` passed
on both arms. r2 C exhausted the 48-round tool budget on op 5
(`edit.replace` 18/11 failed, `process.run` 17/9 failed) after the
workspace already satisfied hidden asserts. That is a tool-loop / last
turn stall, not a missing constraint in the seed.

**Resume operational state.** Both repeats: hidden billing index +
`switch_resume` passed. C forgotten 37 then 46; recovered 0; reread 5–6
(same order as A). C did not treat an old PASS as the current tree hard
enough to fail hidden checks. C r2 spent extra `capability.manage` (12
vs A's 5) and extra rounds (32 vs 18).

**No semantic episode.** Compaction tokens 0/0 on all four cells. C
resident peak 1543 / 1611 vs A 3130 / 5344. Hidden `items[i + 1]` gone
and dump chatter kept. Distill-skip held on this chatter-then-fix shape.

## Working set vs send cost

C final resident bytes stay below A on every pair (C peaks 999–2287 vs
A 3130–5156). C `model_in` is not uniformly lower: extra rounds and
catalog search/load dominate send cost. `reread_warm=0` and
`reread_stored=0` on the printed C cells; recovery rereads are
previously-selected / descriptor-only, not Warm+Stored GC reread.

Do not retune `active_threshold` / `archive_threshold` /
`gc_max_generation`. Do not expand to 27 cells or 300×3. M12 / M13 /
M15 remain open.
