# Longflow after Execution Convergence V1 (2026-08-23)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com). First run r1; a second invocation
the same hour was killed by a provider outage (502/503) and its pair
sink **overwrote the r1 JSONL artifacts** — the numbers below are
reconstructed from the preserved harness log of run 1. Treat every
number as n=1 directional evidence, not an estimate.

## Run 1 (r1, reconstructed from log)

| metric | dynamic (C) | append (A) |
| --- | --- | --- |
| outcome | error — provider HTTP 503 at op 11 | passed |
| rounds | 43 (truncated) | 48 |
| tool calls | 50 | 47 |
| model input tokens | 344247 (lower bound, usage incomplete) | 536355 |
| wall | ~317 s | ~300 s |

Per-tool (C): capability.manage 3/0, context.manage 2/1, edit.replace
7/2, fs.list 3/0, fs.read 16/1, fs.write 2/0, git.diff 2/0,
git.status 2/0, **process.run 9/3 failed**, search.grep 4/0.
Per-tool (A): capability.manage 3/0, edit.replace 12/2, fs.list 2/0,
fs.read 16/1, fs.write 2/0, git.diff 4/0, git.status 6/0,
search.grep 2/0 (no process.run).

## Convergence metrics (new, from ExecutionFrontier events)

| metric | C (43 rounds) | A (48 rounds) |
| --- | --- | --- |
| frontier_advances | 24 | 33 |
| redundant_evidence_calls | 5 | 6 |
| frontier_no_advance_peak | **5 — advisory fired once** | 3 |
| evidence_invalidations | 4 | 9 |
| fs.read motive proto-checkpoint-missing | 6 | 0 |

## Structural comparison vs post-hysteresis baseline
(`../longflow-post-hysteresis-2026-08-23`, C completed 94 rounds)

The honest comparison is per-structural-signal inside the trajectory,
not rounds-across-dirs:

| signal | baseline C (94r) | this C (43r) |
| --- | --- | --- |
| process.run calls / failed | 35 / 20 (18 never-compiled guesses) | 9 / 3 |
| git.status + git.diff re-verifications | 11 + 13 = 24 | 2 + 2 = 4 |
| capability.manage calls | 7 | 3 |
| fs.read proto-checkpoint-missing | 8 | 6 (in a shorter run) |

The executable-guessing loop that dominated the previous gap did not
rebuild in this trajectory (3 failures, no name-variant chain), and
git status/diff re-verification collapsed from 24 to 4 calls. With
C truncated at round 43 these are consistency signals, not proof; the
advisory firing exactly at peak=5 confirms the debt counter works
end-to-end on a live run.

## Run 2 attempt (provider outage, recorded for honesty)

A `--repeats 2` retry started ~30 minutes later died to upstream 502 /
503 on both arms (repeat 1 at round 4/op 2; repeat 2 at round 1 with
zero completion tokens). Its pair sink overwrote the r1 artifacts
described above. A cheap chat-completions probe was set up to rerun
when the provider recovers.

## Verdict

Directional only: no structural no-progress loop rebuilt on either arm
in the healthy portion of run 1, and both new advisory/metric paths
behaved as designed on live traffic. A clean-provider rerun (both arms
completing, ideally repeats=2) is required before any claim about round
deltas; per ROADMAP this gate is about absence of structural loops,
which run 1 supports and does not close.
