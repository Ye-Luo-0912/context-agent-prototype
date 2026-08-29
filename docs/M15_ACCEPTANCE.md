# M15 acceptance design (V1) — frozen semantics; formal evidence reset 2026-08-28

This document is the separately frozen acceptance contract required by
[`ROADMAP.md`](ROADMAP.md) and
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md). It pins the V1
candidate, the evidence planes, the development cells and their verdict
semantics. The three legacy 2026-08-28 attempts are retained for diagnosis,
but none is formal M15 evidence; see
[`evidence/m15-window/REPORT.md`](../crates/agent-eval/evidence/m15-window/REPORT.md).

## 0. Current decision state

Three decisions remain frozen:

1. Budget: 3 tasks × 2 modes × 2 repeats = 12 development cells.
2. Fixtures: `retry_policy_dev`, `retry_diag_dev` and `retry_migrate_dev` as
   semantically defined in `crates/agent-eval/src/m15_pack.rs`.
3. Closure: the `task.complete` lifecycle is a reported product fact, not a
   mandatory M15 V1 pass dimension.

The fixture freeze preserves requested behavior and oracle meaning, not a
known-invalid golden implementation. A golden/self-check repair that only
makes the seed satisfy its already-stated oracle is allowed before source pin;
it must regenerate the pack digest. Any task-contract or oracle-meaning change
requires an explicit acceptance refreeze.

The serving pin was reopened. The earlier Luna and relay attempts cannot select
a serving because their v2 evidence misprojected closure, reused the wrong
pack identity, relied on untyped error text and was summarized by hand. A new
serving may be pinned only after a bounded representative preflight proves the
same endpoint/model/protocol/context-window tuple can start, edit and finish
one fixture. The formal window then uses that one tuple without switching.

That preflight passed on 2026-08-28 and pins PinAI `/v1`,
`gpt-5.6-luna`, Responses, 128,000 tokens. The source-bound dirty-tree
`retry_policy_dev` normal cell passed behavior, diff and closure-required
completion in 26 rounds / 59 calls with zero provider retries. It is not one
of the formal cells and makes no comparative performance claim; its role is
only to prevent spending the clean 12-cell budget on an unproven serving.

The serving tuple remains pinned, but this source-bound product preflight
predates the catalog-cold `fs.mkdir` addition and its admission decision.
The `TOOL-DIR-SURFACE-01` deterministic gate landed (2026-08-28): a typed
missing-parent refusal surfaces exactly `fs.mkdir` with `RecoverySurface`
provenance for one decision, approval unchanged, unrelated missing reads
unaffected. Its 24-cell live run did not promote the recovery source, but a
post-run event audit found zero candidate exposure in either arm. The off/on
round and call differences therefore cannot be attributed to the switch. Keep
the catalog-cold baseline and `with_recovery_surface` off conservatively; the
live decision is `NOT_EXERCISED`, and any future paired report must fail closed
as inconclusive when exposure is zero. Evidence:
[`recovery-surface-gate/REPORT.md`](../crates/agent-eval/evidence/recovery-surface-gate/REPORT.md).

The diagnosis fixture was calibrated 2026-08-29 under the §0 fixture-authoring
provision: the golden solution saturates via `u128` widening, the directive and
golden `DIAGNOSIS` name the saturate-not-wrap edge, the hidden check requires an
overflow-safe marker, fixture self-check executes each pack oracle against the
untouched seed and scripted solution, and the frozen digests are recorded
(diag `2fff51573097fe4c833215420dd0da74f11a645ef5c859bdd9bba87e5b427eeb`;
migrate unchanged). A failing diag cell is therefore an honest result, not a
golden/oracle mismatch.

Three formal, clean, identity-consistent v3 windows now exist and all failed:
`1787966622822` passed 11/12, `1787970773734` passed 9/12, and
`1787973547152` passed 10/12. All have 0 NOT_RUN. Migrate + policy are 24/24
across the windows; formal diag is 6/12 and every failure uses the same invalid
`checked_shl` saturation strategy. This is not a harness or transport defect;
it is a model/solver weakness on the pinned serving. M15 remains open.

The landed Completion Convergence slice does not yet justify another window:
its label is not rendered to the model, eligibility is execution-local rather
than task-aware, tail accounting survives reopened work, and its live runner
has no projection-off treatment arm. Correct those issues and apply the
prospective cross-window rule in §5 before spending another formal window.

## 1. V1 candidate composition

| plane | evidence | status |
| --- | --- | --- |
| Platform gates | M12/M13 clean-tree closure audits in `evidence/platform-closure/{m12,m13}/` | banked |
| Context | frozen `context-mech.v2` 12-cell A/C evidence in `evidence/context-mech/`; no policy retune or rerun | banked |
| Tool Surface | edit-gate v4 archival window plus deterministic crash/race/journal/disk-full coverage; production model surface remains v5; catalog-cold `fs.mkdir` baseline retained conservatively, recovery source behind a default-off switch | banked mechanisms; the `TOOL-DIR-SURFACE-01` deterministic mechanism is green but its 24-cell live run had zero treatment exposure and is not folded into M15; the v5 `task.complete` availability change is not promoted by the invalid M15 attempts |
| Execution coherence | Convergence Bench 4/4 plus `longflow-post-obligation-2026-08-23/` | banked |
| Long-task truth chain | deterministic snapshot fence, unified capture, two-phase completion, verification basis and tuple-only cold resume | banked |
| Advisory switches | `CompletionOpportunity` ended default-off; no candidate switch may be enabled | frozen off |

V1 makes no claim about general task-failure rates. Every plane is a bounded
diagnostic over its named, frozen evidence.

## 2. Formal development window

The live plane is exactly:

- tasks: `retry_policy_dev`, `retry_diag_dev`, `retry_migrate_dev`;
- modes: `normal`, `resume`;
- repeats: two for each task/mode pair;
- engine: the retained C composition;
- source: one clean, unchanged HEAD for all 12 cells;
- serving: one pinned provider/model/protocol/context-window tuple;
- switches: advisory candidates off.

Resume interrupts after the first durably settled mutation and its durable
checkpoint. Continuation must restore and continue from the exact acknowledged
`(lineage, task, sequence, artifact, checksum, capability_generation)` tuple.
A source, pack, host-policy, surface or serving change voids the window.

## 3. Evidence contract

Formal cells use schema `retry-pilot-cell-v3`. Each immutable cell directory
contains the manifest, full event stream, `dimensions.json`, hidden oracle
records and workspace snapshot hash. The dimensions are persisted facts, not
a projection from the exit branch:

- acceptance profile, verdict and observed oracle result;
- behavior and allowed-diff result;
- provider health plus typed error class;
- restored, exact-tuple-matched and continued as independent booleans;
- turn completion and task closure as independent booleans;
- phase counters and runtime error text, including failure exits;
- exact pack id/digest and the shared source/serving/surface identity.

`provider_transport`, `model_output_limit`, `model`, `input_budget`,
`round_budget`, `runtime`, `harness_setup` and `harness_watchdog` are distinct
classes. Provider transport and harness failures are `NOT_RUN`; an incomplete
response caused by `max_output_tokens` is a model-output-limit cell `FAIL`, not
a provider outage.

The harness writes `_windows/<timestamp>/manifest.json` containing the exact
12 claimed cell directories, then re-reads those directories and generates
`REPORT.md`. Report generation refuses mixed identity, missing/duplicate
cells, wrong pack digests, verdict drift, untyped provider health, absent
terminal events, event loss/gaps, summary/dimension drift, escaped cell paths
or a dirty tree. Per-cell and total/max round, tool, wall-time and token facts
come from the persisted event-derived summaries. `agent-eval --m15-report
<window-dir>` rebuilds the same report from persisted facts.

## 4. Verdicts

For the M15 V1 profile:

```text
PASS = behavior PASS
    ∧ allowed diff PASS
    ∧ no typed failure
    ∧ (normal ∨ (restored ∧ exact tuple matched ∧ continued))
```

`task.complete` is reported as `completed | active | failed(reason)` but is
not part of this conjunction. Other evaluation profiles may require closure;
the profile is persisted so the two contracts cannot be conflated.

A cell with provider-transport or harness failure is `NOT_RUN`. Any `NOT_RUN`
censors the whole window. Otherwise the development plane passes only when all
12 cells pass. M15 closes only when every banked plane remains valid, the
development plane passes, the bundles and generated report are committed, and
no acceptance-path defect remains unresolved.

## 5. Freeze rules

Pinned from the first through last cell: source identity, C context
composition, production tool surface, host policy, provider/model serving,
protocol family, context window, pack contents/digests, oracle sources,
acceptance profile and repeat count. No mid-window repair or serving failover
is allowed. Protocol `auto`, fixture filters, non-two repeat counts and
`--allow-dirty` are rejected by the formal command. A censored window remains
auditable but must be rerun whole.

Valid-FAIL windows are different from censored windows. The original freeze
bounded repeats inside a window but omitted a cross-window valid-FAIL retry
budget. The three valid failed windows in §0 remain evidence and must not be
retroactively pooled under a newly invented aggregate. Repeating an unchanged
source/serving until a lucky 12/12 is prohibited.

Prospective rule for the current route:

1. no fourth unchanged v3 window;
2. a materially changed candidate must first pass its deterministic gates, a
   true settlement-projection off/on gate, and an exact-source preflight;
3. that candidate receives exactly one predeclared 12-cell confirmation
   window under §§2–4;
4. a valid FAIL rejects that candidate and returns to diagnosis; only NOT_RUN
   permits a whole-window rerun, because it produced no decision-grade sample;
5. any future aggregate or sequential rule requires an explicit acceptance
   refreeze before observing new cells and cannot reinterpret prior windows.

The context mechanism, GC, retrieval and prompt packing remain frozen. M15 is
an execution/evaluation gate and cannot authorize a context retune.

## 6. Legacy attempts

The three v2 attempts remain immutable raw evidence, but are forensic-only:

- all three packs were stamped with the `retry_policy_dev` identity/digest;
- the evaluator made missing `task.complete` a Runtime failure despite the
  frozen report-only closure contract;
- provider health was inferred from message substrings;
- six relay `max_output_tokens` outcomes were called transport failures;
- the aggregate report was hand-maintained and contains inconsistent counts.

Their individual observations may guide fixture and serving preflight, but
their ratios, cross-window deltas and causal interpretations cannot promote a
surface, reject a serving or close M15.

## 7. Next execution gate

Before spending another 12-cell window:

1. keep the relevant deterministic evaluator/provider/runtime tests green;
2. keep evaluator validity and fixture cleanliness green (done 2026-08-29);
3. correct Task-aware Completion Convergence as specified in
   `LONG_TASK_EVALUATION.md`: task/current-epoch eligibility, fail-closed
   acceptance coverage, bounded default-off projection, episode metrics, and
   deterministic reopening tests; no automatic closure, Context/GC change or
   fixed-round stopping;
4. pass a true same-source projection-off/on normal/resume gate with at least
   two paired repeats, treatment exposure, mandatory-success parity, no lost
   unfinished work and the frozen episode-efficiency criteria — the first
   such gate ran 2026-08-29 (8 cells, `--allow-dirty`; 8/8 cells PASS but
   verdict FAIL: pair-0 exposure asymmetry plus marker needle-shape parity
   in 3/4 pairs and episode medians 1→1), so this item is not yet met and
   the projection stays default-off;
5. freeze and record the candidate identity under the §5 cross-window rule;
6. rerun the bounded one-cell exact-source/product preflight without changing
   the serving tuple;
7. use the preflight-pinned tuple without fallback or automatic protocol
   negotiation, record the exact clean source identity, and set an explicit
   `OPENAI_API_PROTOCOL`;
8. run exactly one uninterrupted, predeclared 12-cell v3 window with
   `agent-eval --m15-window` and accept only its mechanically regenerated
   report.

300×3 scale, `recall_after_fix`, a 27-cell context expansion, a second context
engine comparison and model comparison remain parked until this gate closes.
