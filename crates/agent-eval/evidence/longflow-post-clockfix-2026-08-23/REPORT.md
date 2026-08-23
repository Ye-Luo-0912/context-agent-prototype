# Long-flow after the tool lifecycle clock fix (2026-08-23)

Pack `agent-eval.longflow.v1`, task `late_constraint_long`, r1, live
model, append vs dynamic concurrent. Evidence dir:
`longflow-post-clockfix-2026-08-23/`. Baseline:
`longflow-shakedown-2026-08-22` (pre-clock-fix).

## Dynamic arm

| metric | 08-22 baseline | this run |
| --- | --- | --- |
| rounds | 77 | 84 |
| fs.read | 21 | 26 |
| capability.manage calls | 20 | 20 (5 failed) |
| successful loads | 20 | 13 |
| reloads of already-loaded-once tools | ~12 | 8 |

Reload detail: git.diff x4, process.run x3, git.status x3 — all builtin
optional tools cooled to Warm mid-task and re-loaded later; 5 failed
calls guessed `warm.<tool>` names straight from the catalog's Warm state.

fs.read motives: first=3 body_visible=0 descriptor_only=5
**protocol_checkpoint_body_missing=8** checked_fresh=1 needs_revalidation=9
warm=0 stored=0 changed=0.

## Decisions

1. The corrected clock alone does **not** stop surface churn: with real
   round semantics, optional builtins idle for >8 rounds inside one task
   and re-loading costs a full model round (~8.2K input tokens) while
   keeping a schema costs ~130 tokens/round. Keep-cost < miss-cost, so
   the phase-2 gate from the review is met.
2. Landed in this slice: surface-pressure hysteresis in
   `BuiltinToolDispatcher::gc` — while loaded schema bytes sit under a
   soft high watermark nothing cools; above it, oldest-idle cools until
   the low watermark. Defaults 18_000/9_000 bytes (the whole builtin
   surface is well under the high mark); watermark 0 restores the pure
   idle semantics for tests and scripted fixtures.
3. `protocol_checkpoint_body_missing` showed up live (8 reads), so its
   own gate for the tiny current-turn protocol evidence LRU is now open;
   implementation stays separate and is not part of this slice.

Append arm passed at 47 rounds / capability.manage 6 as usual.
