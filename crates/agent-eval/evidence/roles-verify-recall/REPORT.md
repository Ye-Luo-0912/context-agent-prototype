# NeedVerify roles live compare (`recall_after_fix`)

Not an M15 close. Repeats=1. Same model as the C-hygiene leftover
diagnosis. Cells ran dirty on `105465d` plus the uncommitted execution /
roles tree that this report lands.

- Date: 2026-08-19
- Fixture: `recall_after_fix` (`--compare-live`, arm order rolling →
  dynamic → append)
- Model: `gpt-5.6-luna` via `https://api.pinaic.com/v1` (no API key in
  this tree)
- Binary git_head at run: `105465debdee383e3aa3c3c83a07194fa4d7fb29`
  (`git_dirty=true`)
- Rebuild: `agent-eval --show-evidence crates/agent-eval/evidence/roles-verify-recall/<pinned|unpinned>/recall_after_fix/r1`

Pinned = NeedVerify roles already on the tree, but the eval harness still
force-loaded `git.status` / `git.diff` / `shell.exec` every round.
Unpinned = those tools stay in the catalog and must be loaded through
`capability.manage`. Production NeedVerify prefers a declared `Verify`
role, else capability search, else `EscapeHatch`. `InspectDiff` is never
a verifier. No builtin currently declares `Verify`.

## C versus the last leftover

The C-hygiene diagnosis (`c-hygiene-rounds`, same fixture and model)
left C at **23 rounds / 35 tools** versus A **16r / 17t**. Extra calls
were workspace re-reads and git verify.

| C cell | rounds | tools | git+shell calls | hidden |
| --- | ---: | ---: | ---: | --- |
| C-hygiene leftover | 23 | 35 | present (git verify) | leftover was efficiency, not this report's verify table |
| this tree, harness still pinning git/shell | 15 | 19 | 8 | 7/8 (`main.py` `visit_all`) |
| this tree, unpinned | 14 | 13 | 0 | 7/8 (`scratch.md` `4B`) |

C extra rounds/tools improved. Unpinned C is even cheaper than A on this
n=1 (A 17r/16t). That is not a success-rate win: C still missed one
hidden needle both times.

## Pass table (8 hidden asserts)

| engine | pinned | unpinned | unpinned miss |
| --- | --- | --- | --- |
| append (A) | 5/8 fail | **8/8 pass** | — |
| rolling (B) | 7/8 fail | 5/8 fail | `scratch.md` Breville / 200 / HDMI |
| dynamic (C) | 7/8 fail | 7/8 fail | `scratch.md` `4B` |

Pinned A missed `scratch.md` Breville / 200 / HDMI. Pinned C missed
`visit_all` in `main.py`. Completeness is n=1 trace noise. Do not claim
the role change raised hidden-check success.

## Cost (do not read total `model_in` as savings)

`schema_tokens_total / rounds` is stable per condition: **1578 pinned vs
1333 unpinned** (−245 / round, ~15%). Append unpinned ran 17 rounds
instead of 14, so its totals went up.

| engine | pinned in / r / tools / schema | unpinned in / r / tools / schema |
| --- | --- | --- |
| append | 70537 / 14 / 15 / 22092 | 101314 / 17 / 16 / 22661 |
| rolling | 78936 / 15 / 19 / 23670 | 75599 / 14 / 16 / 18662 |
| dynamic | 87817 / 15 / 19 / 23670 | 70425 / 14 / 13 / 18662 |

git+shell ToolStarted counts: pinned A 7, B 8, C 8; unpinned A/B/C **0**.
`capability.manage` remains `must_surface` / `dispatcher_required` every
round, so the NeedVerify → capability-search fallback is not a distinct
`task_requirement` origin in these traces.

2026-08-14 all-pass numbers (A 9 tools / C 23 tools) are a different day
and the old pinned harness. They are not this A/B.

## Next

Do not retune `active_threshold` / `archive_threshold` / `gc_max_generation`
or reactivation scoring. Do not enable P3/P4. Do not live-run
`semantic_recall.v1`. A declared `Verify` builtin (`tests.run` or similar)
is still missing; production NeedVerify still falls through to capability
search, which is already on the surface.
