# Opportunity gate — item-8 off/on paired live attempt (2026-08-25)

## Scope

First live execution of the ROADMAP item-8 paired gate for the advisory
`CompletionOpportunity` candidate (LT-RUN-04 Slice C). Eight cells over the
frozen `retry_policy_dev` fixture with the C engine (`gpt-5.6-luna` @ PinAI,
Responses, `auto`): {normal, resume} × {candidate off, on} × 2 repeats. The
switch is the only variable; each immutable `dimensions.json` records its
setting plus the per-cell opportunity account (schema `retry-pilot-cell-v2`).

## Cells

| cell | opp | verdict | behavior | diff | closure | rounds | offers | called |
| --- | --- | --- | --- | ---: | --- | --- | ---: | --- |
| normal r1 | off | FAIL | pass | pass | failed | 37+0 | 0 | no |
| resume r1 | off | FAIL | pass | pass | failed | 6+0 | 0 | no |
| normal r2 | off | FAIL | pass | pass | failed | 19+0 | 0 | no |
| resume r2 | off | PASS | pass | pass | completed | 6+28 | 0 | no |
| normal r1 | on | FAIL | fail | pass | completed | 13+0 | 0 | yes |
| resume r1 | on | FAIL | pass | pass | failed | 4+10 | 0 | no |
| normal r2 | on | FAIL | pass | pass | failed | 14+0 | 0 | no |
| resume r2 | on | FAIL | pass | pass | failed | 7+14 | 0 | no |

Provider health was `healthy` in all eight cells (no transport failures).

## Finding 1 — the candidate never armed: zero offers in every on-cell

All six candidate-on cells show `offers=0`; both `called=true` rows came from
the model discovering and calling `task.complete` through the existing
explicit path, not from a derived lease. This pairing therefore did not
measure the candidate's surface effect; it measured model variance plus the
candidate's absence.

Root cause (verified from traces and code, not inferred): the eligibility
precondition requires a trusted verification pass carrying an **exact**
identity. Every live `verify.run` call in these cells used the discovered
`rust.workspace` recipe, and discovered recipes are constructed through
`VerificationRecipe::new`, which defaults to
`VerificationReuse::TaskScoped` with `source_read_only=false`
(`tool-runtime/src/verification.rs`, `push_recipe`). Under TaskScoped
attribution a successful run records a typed verification fact with an empty
identity and emits no `ExecutionVerificationPass` receipt — confirmed by
trace scan: `verification_pass_events=0` in all eight cells despite multiple
successful `verify.run` outputs (e.g. normal/off r1 seq 563,
`rust.workspace passed, exit=0`). The opportunity derivation is fail-closed
on exactly this condition, so zero offers is the specified behavior, not a
defect.

Consequence: an exact-world verifier only exists when the composition root
explicitly registers one (the deterministic replay gate registers such a
recipe and arms end to end). On this fixture no host-registered
ExactCurrentWorld recipe exists, so the candidate is structurally unable to
fire in live cells today.

## Finding 2 — premature-closure counterexample appeared without the candidate

normal/on r1 closed the task (`closure=completed`) while the harness-owned
behavioral oracle failed (`behavior=fail`) — the model reached closure
through its own explicit `task.complete` load with zero derived offers.
This is the known AUDIT concern about closure discoverability versus
non-premature closure, appearing independently of the candidate.

## Finding 3 — accounting anomaly to keep visible

resume/off r1 ended with `error=phase two failed: tool round budget exhausted
after 48 rounds` while the harness counted `rounds=6+0`. The runtime-side
exhaustion message and the harness-side `ModelStarted` counters disagree for
this cell; the trace is retained. Recorded as an open harness-accounting
question, not attributed to any component.

## Attempt 2 — host-registered exact verifier (2026-08-25, same day)

Substrate change (declared, identical across both arms): the pilot
composition root now registers `jobrunner.exact`, a source-read-only
ExactCurrentWorld recipe binding the seed-guaranteed inputs, beside the
unchanged TaskScoped mirror of discovery (`rust.workspace`). A pre-flight
test pins that identity capture succeeds in this environment. Eight fresh
cells ran under `*-attempt2` repeat dirs; attempt-1 bundles are retained.

| cell | opp | verdict | behavior | diff | closure | rounds | offers | called |
| --- | --- | --- | --- | ---: | --- | --- | ---: | --- |
| normal r1 | off | FAIL | fail | pass | failed | 24+0 | 0 | no |
| resume r1 | off | FAIL | fail | pass | failed | 6+0 | 0 | no |
| normal r2 | off | PASS | pass | pass | completed | 21+0 | 0 | no |
| resume r2 | off | PASS | pass | pass | completed | 5+11 | 0 | no |
| normal r1 | on | FAIL | fail | pass | failed | 11+0 | 0 | no |
| resume r1 | on | FAIL | fail | pass | failed | 6+0 | 0 | no |
| normal r2 | on | FAIL | pass | pass | failed | 32+0 | 1 | no |
| resume r2 | on | PASS | pass | pass | completed | 5+29 | 1 | yes |

Provider health was `healthy` in all eight cells.

What worked as designed:

- Arming is now possible and happened: exactly one receipt-backed offer in
  each mode's second repeat
  (`opp/<task>/a0/d1/w18/8da6ede1da6222b7`,
  `opp/<task>/a0/d2/w18/cf1cb53e35cd8e35`), matching one-to-one the two
  cells whose traces contain an `ExecutionVerificationPass` receipt.
- resume/on r2 executed the full intended chain live: offer -> leased
  surface decision -> model called `task.complete` -> committed closure ->
  the cell PASSED every dimension (behavior, diff, closure, restored
  continuation). This is the first live end-to-end proof of the candidate's
  contract.
- normal/on r2 exercised the once-per-basis limit honestly: the model
  ignored the leased decision, was not re-offered, and kept working until
  the round budget ended the turn without closure.

Why the gate still fails to promote:

- Paired outcomes fell: off passed 2/4 versus on 1/4.
- Medians moved the wrong way: normal 24->32 total rounds, resume 16->34.
- Arming remains rare (2 of 6 on-cells): eligibility needs a post-mutation
  re-verification landing as a Current exact pass at a settled batch, and
  the r1 on-cells died earlier instead (two behavioral-oracle failures, one
  event-drain stall).
- One infrastructure flake hit an off cell: resume/off r1 failed loading the
  checkpoint artifact with an exclusive file-lock conflict on
  `.focus-agent/authority/workspace-effects.jsonl` ("the operation would
  block"). Recorded as a real defect candidate for the harness/journal lock
  path, not attributed to either arm.

## Verdict

Both attempts **fail promotion; the candidate remains default-off** and out
of any same-model A/C comparison. Attempt 1 proved nothing measurable
(structural non-arming); attempt 2 proves the mechanism works end to end in
live conditions at least once per mode while failing the promotion criteria
on outcome count and median rounds at n=2. Raising repeats would tighten
noise but does not address the two structural costs this report records: the
rare arming rate and the journal-lock flake. Both are prerequisites before
any future rerun can be decision-grade.

## What would make the gate measurable

~~Before any rerun, the host side must give the fixture an opt-in
source-read-only ExactCurrentWorld recipe (as the deterministic replay does),
so a live `verify.run` PASS can produce an exact receipt and arm eligibility.~~
Done in attempt 2: the registered recipe arms eligibility, and one on-cell
executed the full offer->lease->call->pass chain live.

What remains before a decision-grade rerun:

1. the rare arming rate — models rarely re-verify after their last mutation
   with nothing else pending; whether to surface verification demand
   differently is a gated design question, not something this report changes;
2. ~~the journal-lock flake on checkpoint artifact load~~ fixed
   (2026-08-25): `WorkspaceEffectJournal::open` now retries the exclusive
   lock with bounded backoff, since a predecessor runtime releases its
   handle asynchronously and a quick reopen raced that window;
3. more paired repeats once (1) is decided, since n=2 medians stay
   noise-dominated.
Whether general discovered runners should ever be upgraded is a separate,
gated decision; this report does not propose retuning them.
