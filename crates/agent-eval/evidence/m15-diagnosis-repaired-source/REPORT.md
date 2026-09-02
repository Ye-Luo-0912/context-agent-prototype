# Repaired-source M15 v4 window failure diagnosis (2026-09-03)

Post-window evidence analysis of the fifth formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788385151733`, predeclared
clean source `43e1033`, per-cell source tree digest `fd9799c9...`),
following the M15_ACCEPTANCE §5 return-to-diagnosis from its valid FAIL
(6/12). Serving tuple: PinAI (`https://api.pinaic.com/v1`), `gpt-5.6-luna`,
Responses, explicit protocol, 128,000-token context, 4,096 max output
tokens. All facts are read from the immutable per-cell event streams,
per-cell `summary.json` and the harness `verify.json` records; no new live
run produced them.

Verdict shape: 6/12 pass, 0 NOT_RUN, provider healthy in every cell,
behavior pass 8/12. Migrate 4/4 (all 10–11 rounds, clean), diag 1/4,
policy 1/4. Rounds 243 total / max 53; tool calls 488 / max 125; wall max
504,292 ms; provider input 2,441,871 / output 81,827 (cached 217,600).

The post-window route requires the three failure observations below to be
kept distinct until typed evidence proves a shared cause. They are: the
diag overflow-edge misses, the policy completion-gate tails, and the
bounded-framer malformed-event. No harness, transport or oracle defect was
found in any class; the observations remain three separate model-quality
surfaces.

## 1. Diag overflow edge — same trap shape, third serving, 1/4

| cell | rounds | backoff.rs shift shape | verdict |
| --- | ---: | --- | --- |
| `retry_diag_dev-normal/r1-attempt11` | 7 | `checked_shl(exponent)` | fail |
| `retry_diag_dev-normal/r2-attempt11` | 8 | `checked_shl(shift)` + `min(max_delay_ms)` | fail |
| `retry_diag_dev-resume/r1-attempt11` | 10 | `checked_mul(factor)` + `min(max_delay_ms)` | pass |
| `retry_diag_dev-resume/r2-attempt11` | 47 | `checked_shl(shift)` + `min(max_delay_ms)` | fail |

The hidden oracle's single failing assertion in every failing cell is
`src/backoff.rs` "shift corrected and overflow-safe"; all other assertions
(diagnosis document, public signature, seed-table removal, untouched
config/README) pass. Every failing cell wrote a `checked_shl` variant,
which only guards shift counts ≥ 64 and still loses bits shifted out
(`100u64.checked_shl(62)` is `Some(0)`); the passing cell wrote the
`checked_mul` shape the calibration rewards. This is the identical trap
shape attributed across all three v3 windows (6/6 failing diag cells) and
the earlier diag smoke evidence. The fixture, oracle and needles are
unchanged; the trap recurs on a third serving (PinAI Luna after the relay's
v4-flash). Conclusion: a persistent model/solver limitation on the overflow
edge, not a harness or serving defect.

## 2. Policy completion-gate tails — the 784d7aa loop recurs, 1/4

| cell | rounds | `task.complete` | refusal reasons observed |
| --- | ---: | --- | --- |
| `retry_policy_dev-normal/r1-attempt11` | 48 | refused 2/2 | "1 unresolved failed command(s) remain"; then "trusted verification is not current; 1 acceptance criterion/criteria lack current coverage; a concrete next action remains; 1 unresolved failed command(s) re…" |
| `retry_policy_dev-normal/r2-attempt11` | 20 | 1 refused, 1 accepted | — (closed) |
| `retry_policy_dev-resume/r1-attempt11` | 9 (phase two cut) | n/a | died on the framer cap, see §3 |
| `retry_policy_dev-resume/r2-attempt11` | 53 (phase two 48) | refused 6/6 | the same four-part compound reason on all six attempts |

Facts from the immutable streams:

- The functional work is done in both failing cells: behavior pass, diff
  pass, the workspace satisfies the directive, and trusted `verify.run`
  PASSes are recorded with acceptance receipts for criterion 0
  (`retry-policy-public-contract`, exact equivalence).
- `normal r1`: the persistent failed command is one `process.run` that
  completed with exit=1 (44 output lines). Later trusted PASSes did not
  produce a converging tail: the model answered the first refusal with more
  `edit.patch` churn (14 calls, 7 failed — including two late
  `ambiguous_match` refusals, each opening a fresh failed-command row), one
  `task.manage` refusal for an oversized `next_action` (200-char bound),
  repeated `verify.run` PASSes, and a second completion proposal only after
  another successful patch had staled the verification again. It never
  reached the tail the gate requires (zero failed-command rows, current
  verification, empty `next_action`) before the 48-round budget.
- `resume r2`: heavy tail churn — `git.diff` 23, `git.status` 12,
  `verify.run` 15, `task.complete` 6 (all refused), `edit.patch` 6 (1
  failed) — then the same 48-round exhaustion in phase two.
- The completion-repair stage is live (refusal bodies state "Runtime
  re-derives the current completion_repair/v1 stage in TASK PROGRESS"), so
  the repaired source surfaced the repair plan; the model still did not
  convert it into the converging action sequence on this serving.
- The loop is stochastic, not deterministic: `normal r2` on the same
  source/serving closed in 20 rounds with the second `task.complete`
  accepted.

Conclusion: the completion gate refused on its declared grounds every
time; this is model task-execution behavior (churn that repeatedly
re-opens one blocker while closing another), the same loop diagnosed at
`784d7aa`, now observed on the repaired source with the repair stage
active. Not a transport, gate or oracle defect.

## 3. Bounded-framer malformed-event — new surface, guard held

`retry_policy_dev-resume/r1-attempt11` died in phase two on
`model protocol error (malformed-event): buffered model stream exceeded
its limits (chunks 16385/16384, bytes 1742733/16777216)`. The cell had 9
model rounds of ordinary small-delta work (428 chunks total before the
final call, tools all small reads plus 2 `edit.patch`); the terminal call
emitted a pathological delta stream that crossed the bounded framer's
chunk cap by one chunk. The guard cut the stream, classified
`malformed-event`, recorded `turn_cancelled` plus one `failure` event, and
the cell failed closed — the designed behavior for a runaway stream. This
surface had not occurred in any previously banked window. It is a model
wire-quality/production failure; the harness bounds held and there is no
retry path by design (wire damage stays non-retryable).

## 4. Cross-class assessment

The three classes are distinct: different fixtures, phases, tools and
error classes. Nothing in the streams links them to one cause, and no
class implicates a harness, transport or oracle defect:

- the diag needles and oracle agree with the frozen calibration, and the
  passing diag/migrate/policy cells prove the gate path healthy;
- every completion refusal matches its recorded ledger state;
- the framer cap fired exactly at its bound and failed closed.

M15 remains open. Per M15_ACCEPTANCE §5 this valid FAIL rejects the
repaired-source + PinAI/Luna candidate; the window is not rerun. Candidate
selection is a user decision; the facts above bound the decision space:
the deterministic gate chain is fully green on this source, so the
remaining failure surface is the model's per-cell behavior on all three
classes (serving/model quality) versus any future harness-visible
candidate that changes the completion tail without prompt pressure,
round stops, Context/GC retuning or protocol-bound weakening — none of
which the current route authorizes.
