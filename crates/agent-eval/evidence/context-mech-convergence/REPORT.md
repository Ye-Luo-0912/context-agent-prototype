# context-mech-convergence live (4 cells): late-semantic op5 reproduction

Not an M15 close. Not a GC retune. Not a Context result. This is the
三十六 follow-up to `context-mech.v2`: reproduce the production-surface
`late_semantic_constraint` r2 C 48-round tool loop in isolation and see
whether it survives the Execution Convergence machinery
(MOD-OBS-01 refusal-as-observation, MOD-PROG-01 stall advisory +
deterministic duplicate refusal, TurnFrame wire checkpointing).

- Date: 2026-08-21
- Command: `agent-eval --repeats 2 --evidence-dir
  crates/agent-eval/evidence/context-mech-convergence --context-mech-run
  late_semantic_constraint`
- Evidence: `crates/agent-eval/evidence/context-mech-convergence/`
- Cells: A/C × 2 repeats = **4** (live, production
  `ToolLifecycleConfig::default()`, no scripted surface pins)
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in
  this tree)
- Binary `git_head` at run: `65c64638605d1fb7227143659b95ba39cfb6745a`
  (tree clean at start). First cell `git_dirty=false`; later cells
  `git_dirty=true` — the run's own untracked evidence output entered the
  identity scan (self-pollution). Post-run fix: the identity scans now
  exclude `crates/agent-eval/evidence` (run outputs are not tested
  sources). The manifests record what actually happened.
- Wall: ~14 min
- Rebuild: `agent-eval --show-evidence
  crates/agent-eval/evidence/context-mech-convergence/late_semantic_constraint/r<n>`

## Pass table

Hidden file+command checks passed on **all 4 cells**; ITT `outcome` is
pass on 4/4. No cell approached the 48-round cap.

| task | r | arm | hidden | in / rounds / tools | wall |
| --- | ---: | --- | --- | --- | ---: |
| late_semantic_constraint | 1 | A (append) | pass | 125255 / 17 / 18 | 120 s |
| late_semantic_constraint | 1 | C (dynamic) | pass | 301235 / 40 / 50 | 277 s |
| late_semantic_constraint | 2 | A (append) | pass | 131882 / 18 / 15 | 144 s |
| late_semantic_constraint | 2 | C (dynamic) | pass | 213145 / 29 / 28 | 293 s |

## The reproduction target did not recur

The `context-mech.v2` r2 C cell was `error` at the 48-round cap
(60 rounds / 70 tools / 852K input; `edit.replace` 18 calls / 11
failed, `process.run` 17 calls / 9 failed). In this run **r2 C passed
in 29 rounds / 28 tools / 213K input**.

The failure *environment* is still present — r2 C still hit
`no_exact_match` ×8, `missing_project_marker` ×2, `path_not_found` ×1
(`edit.patch` 4/4 failed, `edit.replace` 5 calls / 4 failed,
`process.run` 2/2 failed) — but the identical-retry pileup did not
happen: no tool was retried more than a handful of times, and **no
`duplicate_no_progress` refusal appears in any trace** (the
deterministic duplicate refusal never had an identical retry to
refuse).

## Post-run edit-contract audit (2026-08-22)

The edit failures above were not all stochastic. A trace+seed replay
across these four cells found `edit.patch` **5/5 failed** and
`edit.replace` **8/21 failed**. All 11 `no_exact_match` refusals
(`edit.patch` 5 + `edit.replace` 6) used a multi-line `old` value. The
seed `src/protocol.rs` is CRLF (290 bytes; 16 CR + 16 LF), while
`fs.read` rendered it through `str::lines()` and therefore showed the
model LF-only text. The edit tools then matched that LF text against
the raw CRLF string. Raw match count was zero; treating only LF/CRLF as
equivalent made every single hunk unique, and the four-hunk patch
applied fully in order.

This is a deterministic Tool Surface contract defect, not a Context
or GC result. The post-evidence fix preserves raw-byte revisions and
exact whitespace matching, adapts LF/CRLF only for uniform target
files, and preserves the target EOL on write. It was not present in
these recorded cells; a later Tool Surface rerun is required before
claiming a first-attempt-success improvement.

## Reading (bounded)

1. Persistence of the op5 loop is stochastic: with the same task,
   surface and engine, 4/4 cells completed and the previously failing
   repeat passed at 29 rounds. That does **not** make the failure
   environment clean: the LF/CRLF `no_exact_match` path above was a
   deterministic runtime defect.
2. These 4 cells **did not exercise the new convergence machinery**:
   no duplicate refusals fired, and no stall signature accumulated 3
   consecutive no-progress rounds. The machinery stays unproven by
   this run — it fired zero times.
3. Do not read C's higher round/tool counts (40/50, 29/28) vs A
   (17/18, 18/15) as a Context regression: live traces diverge and
   `model_in` is not a same-trace A/B comparison (same caveat as
   `context-mech.v2`).

## Next (unchanged mainline)

This closes the 三十六 reproduction ask. Convergence machinery
validation still needs a trace where the loop actually forms —
a synthetic deterministic harness (scripted model that repeats one
refused edit) is the cheap next step, not more live cells. M12 / M13 /
M15 remain open. Do not retune `active_threshold` /
`archive_threshold` / `gc_max_generation` from this run.
