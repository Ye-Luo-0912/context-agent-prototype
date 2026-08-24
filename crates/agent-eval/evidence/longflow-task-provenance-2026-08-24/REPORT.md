# Execution amplification audit: receipts and task provenance (2026-08-24)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com), production tool surface. The first four
diagnostics use one repeat; the final standing-instruction candidate uses two.
Each row below is a separate dirty-tree development diagnostic with a distinct
`source_tree_digest`; it is not a release gate or a stable stochastic effect
estimate. Every listed arm-run passed all hidden file-content and command
assertions.

## Candidate sequence

| candidate | C rounds / calls | A rounds / calls | C / A model input | result |
| --- | ---: | ---: | ---: | --- |
| exact body coverage baseline | 51 / 64 | 40 / 36 | 434171 / 420554 | retained; prior report |
| bounded turn-checkpoint receipts | 49 / 65 | 38 / 29 | 395323 / 386112 | retained as bounded protocol correctness; no convergence claim |
| raw cross-turn task-change rows | 55 / 70 | 49 / 49 | 451059 / 577540 | rejected |
| refined task-change provenance label/dedup | 127 / 174 | 47 / 39 | 519936 / 283003 | rejected and reverted |
| current-workspace authority prompt, r1 | 64 / 79 | 44 / 30 | 225682 / 241104 | rejected |
| current-workspace authority prompt, r2 | 72 / 76 | 43 / 29 | 255657 / 216689 | rejected and reverted |

The first row is documented in
`../longflow-body-coverage-2026-08-23/REPORT.md`. The other artifacts are in
`../longflow-turn-receipts-2026-08-24/`,
`../longflow-task-changes-2026-08-24/`, and this directory. Their dynamic and
append manifests stamp `tool_surface=production`; all summaries report
`passed=true`.

The final two paired repeats are under
`../longflow-workspace-authority-2026-08-24/`. All four arm-runs also passed
hidden verification. The candidate added one short generic standing rule:
treat the current workspace as authoritative and do not investigate change
provenance unless requested. It was removed after the predeclared round/call
and long-tail gate failed.

## What remains in the implementation

The bounded checkpoint receipt index remains. When a model-facing TurnFrame
omits old complete exchanges, its deterministic checkpoint can include at
most six distinct recent persistable outcomes. Each row contains only tool
name, status, and a tool-owned summary capped at 96 characters. Arguments,
raw/model content, artifacts, and transient context retrieval are excluded.
The full runtime TurnFrame is untouched. `ModelStarted.turn_checkpoint`
records counts only.

This mechanism activated in only one C round in its live diagnostic: one
compacted exchange and four receipts; A did not activate it. It therefore
cannot explain, or claim to fix, the whole-run C/A gap. It is retained because
it makes protocol compaction bounded and less lossy without adding a planner,
cross-turn transcript, Context item, or authority.

## Rejected cross-turn hint

The task-change candidate added current task-owned `path@digest turn=N` rows
to TaskProgress. Its first version shortened the already-satisfied decode turn
from 8 rounds / 18 calls to 5 / 11, but whole-run C still worsened to 55 / 70.
A label/dedup refinement then exposed a severe global coupling: the
constraint-only turn grew to 41 rounds / 69 calls and the decode turn to 33 /
49, taking C to 127 / 174 while A stayed 47 / 39. The trace shows repeated
context, git, process, completion, and task-control activity rather than a
missing file body.

The local attribution improvement was therefore not causal proof of a general
optimization. Model-visible provenance changed prompt/control behavior across
unrelated turns. `TaskProgress.task_changes`, `last_task_change_turn`, its
event counters, prompt text, tests, and contract prose were reverted. This is
an evidence-backed rejection, not an unfinished rollout.

## Rejected standing instruction

The baseline trace did expose a general behavior: after the exact `fs.read`
already showed `decode -> Result`, C used git history/status/diff and command
execution to attribute the existing change, while A reported the current
state. A short system-policy candidate explicitly said to trust current
workspace state and skip provenance unless the task requested it. It kept all
tools and caps available and did not touch Context.

The paired result rejected the candidate. C ran 64 / 79 and 72 / 76 versus A
at 44 / 30 and 43 / 29. The two C runs used `task.complete` 7 and 5 times;
multiple ordinary mutate/test turns, not only the already-satisfied decode
turn, grew longer. The rule therefore coupled to completion, capability, and
verification behavior instead of only removing provenance work. Hidden
success alone was insufficient: median C rounds/calls increased and new
long-tail turns appeared. The prompt and its architecture prose were reverted.

## C still has a context-plane advantage

Use the retained-receipt diagnostic as the closest live measurement of the
final implementation:

| context / prompt metric | C | A | C delta |
| --- | ---: | ---: | ---: |
| historical-context tokens | 47512 | 99282 | -52% |
| selected resident + reactivated tokens | 24280 | 71797 | -66% |
| final resident bytes | 5170 | 18893 | -73% |
| model input per used round | 8068 | 10161 | -21% |
| total used-round model input | 395323 | 386112 | +2% |
| TurnFrame tokens | 32720 | 13562 | +141% |
| active tool-schema tokens | 52210 | 30278 | +72% |

C's dynamic working set remains materially lighter than append history. The
advantage is real at the context layer, but 11 extra model rounds and 36 extra
tool calls consume it at task level. The refined rejected run demonstrates why
context efficiency alone is not a success metric: C still held a small
resident set while execution exploded.

## Outcome-frontier and optional-surface reaggregation

After the retained implementation was restored, the same immutable
turn-receipt event streams were reaggregated with two event-only shadow
diagnostics. They do not change Runtime behavior, Context selection, or tool
availability:

- the outcome shadow advances only for a successful Known mutation result, a
  host-stamped typed verification result, or a committed `TaskCompleted`;
- the optional-surface ledger joins bounded `ToolSurfacePlanned.selected`
  provenance to the following model-requested calls. Both reports had zero
  selected-row truncation, so the reported-row counts are complete here.

| execution diagnostic | C | A | C - A |
| --- | ---: | ---: | ---: |
| outcome advances | 8 | 8 | 0 |
| successful Known mutation results | 8 | 8 | 0 |
| typed verification results | 0 | 0 | 0 |
| committed task completions | 0 | 0 | 0 |
| evidence-only results | 48 | 21 | +27 |
| Unknown-footprint invalidations | 9 | 0 | +9 |
| max consecutive results without outcome advance | 18 | 3 | +15 |
| rounds exposing catalog-loaded optional rows | 39 | 28 | +11 |
| catalog-loaded optional rows exposed | 134 | 28 | +106 |
| optional schema tokens exposed | 10388 | 2184 | +8204 |
| optional requested calls | 18 | 2 | +16 |
| exposed optional rows not requested that round | 118 | 26 | +92 |

Because neither arm emitted a typed verification, this is an exact partition
of completed tool results in these traces:
`8 + 48 + 9 = 65` for C and `8 + 21 = 29` for A. Both arms performed the
same six `edit.patch` and two `fs.write` outcomes. The complete 36-call gap is
therefore execution exploration/control in this diagnostic, not additional
necessary file mutation. It would be incorrect to label every observation as
waste in arbitrary tasks; the stronger fact is that Runtime had no typed
verification or task-completion result against which to attribute necessity.

The first causal split is turn 7, after both arms had exact file-body evidence
that `decode` already returned `Result`. A finished after three observations.
C accumulated 18 results without an outcome: it loaded Git and shell, queried
new paths/arguments, and inspected history. New `list`/`read`/`grep`/Git
observations were classified `EvidenceAdvanced`, so the existing evidence
frontier repeatedly cleared its stall debt even though the task predicate did
not move. Nine successful generic `shell.exec` calls were conservatively
`MutationFootprint::Unknown`; they invalidated current resource facts, after
which fresh reads became new evidence and cleared the evidence debt again.

The optional-tool lifecycle amplified that loop. Production's 18 KiB schema
high watermark exceeds the complete builtin schema set, so a catalog tool
loaded once normally remains visible across later user turns. In this trace C
loaded Git/shell during turn 7 and continued to expose them; A did not enter
that path. The watermark accounts for schema bytes but not the trajectory cost
of making an affordance salient. Lowering the byte watermark globally is not
a supported fix: it would reintroduce unconditional load/unload churn.

### General algorithm candidate (not landed)

Keep the current Evidence Frontier for freshness and explanation. Add a
bounded, host-attributed execution layer rather than treating every new fact
as task progress:

1. `ExecutionAttribution` distinguishes a pre-existing typed verification or
   provenance obligation, exact/equivalent verification reuse, completion
   control, and unattributed observation. Attribution is fixed before a call;
   a successful command cannot retroactively declare itself necessary.
2. A `VerificationLease` is keyed by task/anchor/obligation revision, relevant
   input identities, execution-profile revision, trusted recipe id/revision,
   and canonical arguments. Only a host-registered verifier can issue or
   settle it. Generic shell/process metadata cannot mint a reusable PASS.
3. Identical or host-declared equivalent current PASS leases are reused;
   concurrent requests join one bounded in-flight lease. A changed relevant
   input or environment creates a new epoch. Incomplete input coverage widens
   to Workspace rather than truncating and claiming validity.
4. A catalog `CapabilityLease` exposes a loaded optional schema for the
   current user turn by default. Core tools, active calls, explicit
   `TaskToolRequirement`s, mutation primitives, and tools tied to unresolved
   typed obligations remain rooted. At the next turn an unrooted tool returns
   to the discoverable catalog/Warm state; `capability.manage` remains visible
   and can reload it, so initiative is not hard-blocked.
5. Every model-requested action receives a small body-free execution receipt,
   including transient capability/context queries. Context persistence stays
   orthogonal: a transient search still does not become a `ContextItem`, but
   it can no longer disappear from convergence accounting.
6. A path miss first creates a revision-bound negative fact. It opens a
   blocker obligation only when the path is rooted in the current directive,
   an acceptance requirement, a mutation precondition, or an explicit task
   requirement. A speculative `Cargo.toml` probe must not become a permanent
   obligation in a non-Cargo workspace.

Suggested implementation bounds are eight verification leases, 32 coverage
atoms, four provenance leases, four probes per provenance provider, and one
pending completion. State contains only ids, digests, enums, counters and
artifact refs; raw command output remains stored once as an artifact.

The shadow `outcome_frontier` and `optional_surface(reported)` counters are now
implemented in `agent-eval` and emitted by `--metrics`/new bundles. They are
measurement support, not a runtime planner or a claim that the candidate above
has passed its gate.

## Decision and next gate

- Keep exact body coverage, `EvidenceReconfirmed`, production-surface
  stamping, and bounded checkpoint receipts. None is reported as closing the
  round/call gap.
- Keep Context selection, GC, reactivation scoring, residency, and budgets
  frozen. The data gives no reason to trade away C's context advantage.
- Do not add another model-visible progress/provenance hint directly to the
  live path, and do not substitute a generic "stop earlier" standing prompt.
  First build a deterministic already-satisfied-task replay, then require at
  least two paired live repeats. Acceptance requires lower median rounds and
  calls, unchanged hidden success, and no new long-tail constraint turn.
- The next investigation is execution-only: attribute optional capability
  activation and repeated verification/completion after the first exact body
  proves the change already exists. The first measurement layer above has now
  landed. Before changing surface leasing or completion semantics, add the
  deterministic lease/reuse/negative-fact tests, then run at least two paired
  live repeats. Do not reduce the round cap or suppress model initiative as a
  substitute.

M12, M13, and M15 remain open.
