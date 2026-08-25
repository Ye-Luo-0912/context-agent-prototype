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

## Verdict

**The promotion gate fails: the candidate remains default-off** and must not
enter any same-model A/C comparison. The paired medians (normal 37→14,
resume 34→21 total rounds) carry no evidential weight at n=2 with zero
offers armed and mixed behavioral outcomes.

## What would make the gate measurable

Before any rerun, the host side must give the fixture an opt-in
source-read-only ExactCurrentWorld recipe (as the deterministic replay does),
so a live `verify.run` PASS can produce an exact receipt and arm eligibility.
Whether general discovered runners should ever be upgraded is a separate,
gated decision; this report does not propose retuning them.
