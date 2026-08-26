# Tool-surface-edit v3 clean-tree run — 2026-08-26

Two live runs of the frozen `agent-eval.tool-surface-edit.v3` pack
(4 fixtures × 3 repeats, production-default surface, dynamic engine) on a
clean source tree, provider PinAI `gpt-5.6-luna` direct. This is the run
class that TOOL-EDIT-02 requires; the gate did not close and both runs are
recorded here.

## Run 1 — gate/schema contract drift (fail 0/12, strict 12/12)

Every cell: strict raw-byte truth passed, hidden verification passed, but
the flow gate rejected all cells with "first edit.patch call did not use the
fixture's exact local hunks". Event inspection showed the model sending
`"op":"replace"` on every hunk — correct per the current model-visible
schema (an explicit `op` became required after this gate was authored), but
the gate whitelist accepted only `{old,new,occurrence}`.

Fix landed with this bundle's commit: the gate accepts exactly the runtime
enum values (`replace`/`insert_before`/`insert_after`) plus the legacy
omitted-op spelling; fingerprints remain `(path, old, new)` so fixture
expectations are unchanged. Regression test:
`hunk_validation_accepts_schema_required_op_and_legacy_omission`. Run 1's
raw bundle is archived out of tree at
`target/eval-evidence-archive/schema-drift-diagnostic-2026-08-26/` (not
committed).

## Run 2 — post-fix (strict 11/12, gate 9/12, non-conflict-first 7/9)

Verdict line: `verdict=fail identity=true strict=11/12 gate=9/12
non_conflict_first=7/9 stale proactive/reactive=1/1 rounds=43 wall_ms=1277194
tokens=265568 usage_incomplete_cells=1`.

Per-cell outcomes:

| Cell | Strict | Gate | First exact/green | Note |
| --- | --- | --- | --- | --- |
| crlf_multi_hunk r1 | pass | fail | false/false | first attempt used revisions not from latest reads; recovered on attempt 2 |
| crlf_multi_hunk r2/r3 | pass | pass | true/true | |
| mixed_eol r1–r3 | pass | pass | true/true | |
| stale_revision_recovery r1 | pass | pass | true/true | |
| stale_revision_recovery r2 | pass | fail | true/true | one post-edit confirmation read; fixture forbids confirm reads |
| stale_revision_recovery r3 | pass | pass | true/false | attempts=2 (recovered) |
| batch_two_file r1 | pass | pass | true/true | |
| batch_two_file r2 | **fail** | fail | — | provider session ended before any tool call (`usage_incomplete`, rounds=1, zero patch calls), wall 300 s |
| batch_two_file r3 | pass | pass | true/true | |

Provider latency was elevated for this window: total wall 1277 s versus 463 s
for the r4 diagnostic, with one hung session.

## Conclusions

- The op-field drift was a real frozen-gate/schema mismatch and is fixed.
- No runtime regression is claimed: 9/12 cells fully green including all
  mixed_eol cells; both non-green strict failures trace to provider session
  loss, not tool behavior.
- TOOL-EDIT-02 stays open: acceptance needs 12/12 strict, 12/12 gate and
  9/9 non-conflict first patch in one stable provider window.
