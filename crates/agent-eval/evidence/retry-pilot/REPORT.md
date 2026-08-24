# retry_policy_dev live pilot (layer 2) — first C-engine cells

Date: 2026-08-25 · Engine: C (`dynamic` = SimpleContextEngine +
ModelBackedCompactor) · Tool surface: production `ToolLifecycleConfig::default()`
· Model: `gpt-5.6-luna` @ `https://api.pinaic.com/v1` (eval.env identity)
· Fixture/directive: frozen `retry_policy_dev` (see
`agent-eval::long_task`; digest recorded per cell manifest)
· Runner: `agent-eval --long-task-live [normal|resume]`
(source: commits "Add the retry_policy_dev live pilot runner",
"Wire the checkpoint store into the live pilot composition",
"Make the live resume interruption operator-shaped and fix diff paths").

## Status

**Harness validated; no acceptance claim.** Per
[`LONG_TASK_EVALUATION.md`](../../../docs/LONG_TASK_EVALUATION.md) these
cells validate the harness and are not acceptance. All four canonical
cells FAIL, one of them on a since-fixed harness bug, three on the same
model-behavior finding.

## Canonical cells (third run)

| Cell | rounds (p1+p2) | resume/durable | TaskCompleted | cargo oracle | verdict |
| --- | --- | --- | --- | --- | --- |
| normal r1 | 13+0 | 5/5 | no | skipped (cell errored) | FAIL — turn ended without closure |
| normal r2 | 24+0 | 9/9 | no | skipped | FAIL — turn ended without closure |
| resume r1 | 6+17 | 8/7 | no | skipped | FAIL — continuation ended without closure |
| resume r2 | 6+15 | 5/5 | no | skipped | FAIL — continuation ended without closure |

Provider input/output tokens per cell (in/out): 68.5k/3.6k, 136k/4.7k,
145k/5.3k, 109k/5.3k. Wall time 149–174 s.

Earlier attempts are retained unmodified under their `-attempt{k}`
directories per EVAL-IMMUTABLE-01, including two transport-dead cells
(`error sending request for url … api.pinaic.com`) from the second run —
the PinAI instability STATUS warns about.

## What the pilot proves (harness side)

1. **Live stop/restore/continue works end to end.** Both resume cells
   interrupted on the semantic trigger (first durably settled workspace
   mutation + its durable checkpoint), cancelled the in-flight turn like
   an operator stop, captured the runtime checkpoint, restored a fresh
   instance through the shared durable authority lineage, continued the
   SAME directive via `continue_active_task`, and ran 15–17 further
   rounds. Event shape per cell: `turn_cancelled` → restore →
   `task_continuation_started` → work → `turn_completed`, with durable
   checkpoints landing throughout both phases.
2. **Durable checkpoints require the workspace store.** The first live
   attempt failed every scheduled write with "no checkpoint store
   configured" because the composition omitted `artifact_store`. Fixed;
   later cells show paired resume-commit/durable flows.

## Model-behavior finding (not tuned, recorded)

In all four canonical cells the model finished implementation, verification
and documentation work and then ended its final message with a report
instead of closing the task through intent-gated `task.complete`. The
discoverability gap is narrower than "no closure calls": none of the four
canonical cells even loaded `task.manage` or `task.complete` through
`capability.manage` — every catalog-control call fetched operational tools
(`shell.exec`, `process.run`, `edit.replace`). Because the evaluator of this
run returned on the lifecycle error before the post-run checks, the canonical
cells carry no independent behavioral verdict; their assistants' self-reports
of passing tests are recorded but unverified. This run's evaluator skipped
the oracle in that situation; LT-RUN-04 Slice A replaces that behavior with
independent per-dimension scoring.

One retained earlier attempt (`normal/r1-attempt2`, second run) shows the
full path is reachable without any harness change: the model discovered
`task.complete` through the catalog (escape-hatch role), loaded it, closed
the task and passed the post-run cargo check (6 tests plus doc-tests); its
cell verdict is FAIL only because of the since-fixed Windows diff-path bug.

No prompt or gate was changed to improve closure rates; whether closure
guidance belongs in the product prompt is a decision point for the runtime
owners, now framed as the default-off `CompletionOpportunity` candidate in
LT-RUN-04.

## Verdict rules used

passed = TaskCompleted observed AND hidden `cargo test` passes with ≥1
executed test AND the finished diff stays inside `src/**`, `tests/**`,
`README.md`, `Cargo.toml` (seed deletions violate). Layer-1 marker
predicates are reported per cell as diagnostics, never gating. The oracle
is skipped, and recorded as skipped, when the cell already errored.

## Next

Same-model A/C normal/resume pairs remain open behind the
provider-stability caution (two transport failures inside one hour here).
The completion-closure variance should be decided as a product/prompt
question before acceptance-style cells are counted.
