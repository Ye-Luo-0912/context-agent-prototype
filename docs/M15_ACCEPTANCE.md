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
   implemented in `crates/agent-eval/src/m15_pack.rs`.
3. Closure: the `task.complete` lifecycle is a reported product fact, not a
   mandatory M15 V1 pass dimension.

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
unaffected. After its isolated live paired gate freezes the exact product
surface, rerun the same one-cell bounded preflight on that source before the
formal window. This is product/source readiness confirmation, not permission
to switch the serving tuple or alter this acceptance contract.

No formal M15 development window currently exists. M15 remains open.

## 1. V1 candidate composition

| plane | evidence | status |
| --- | --- | --- |
| Platform gates | M12/M13 clean-tree closure audits in `evidence/platform-closure/{m12,m13}/` | banked |
| Context | frozen `context-mech.v2` 12-cell A/C evidence in `evidence/context-mech/`; no policy retune or rerun | banked |
| Tool Surface | edit-gate v4 archival window plus deterministic crash/race/journal/disk-full coverage; production model surface remains v5 while catalog-cold `fs.mkdir` awaits its isolated live admission gate | banked mechanisms; the `TOOL-DIR-SURFACE-01` deterministic gate landed (2026-08-28) and its live paired gate is not folded into M15; the v5 `task.complete` availability change is not promoted by the invalid M15 attempts |
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

Before spending a 12-cell window:

1. keep the relevant deterministic evaluator/provider/runtime tests green;
2. finish the `TOOL-DIR-SURFACE-01` isolated live paired gate outside M15
   and rerun the bounded one-cell
   source/product preflight without changing the serving tuple;
3. use the preflight-pinned serving tuple in §0 without fallback or automatic
   protocol negotiation;
4. record the exact serving and clean source identity;
5. set an explicit `OPENAI_API_PROTOCOL` and run one uninterrupted 12-cell v3
   window with `agent-eval --m15-window`;
6. accept only the mechanically regenerated report.

300×3 scale, `recall_after_fix`, a 27-cell context expansion, a second context
engine comparison and model comparison remain parked until this gate closes.
