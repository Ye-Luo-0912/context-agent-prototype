# M15 acceptance design (V1) — frozen semantics; formal evidence reset 2026-08-28

This document is the separately frozen acceptance contract required by
[`ROADMAP.md`](ROADMAP.md) and
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md). It pins the V1
candidate, the evidence planes, the development cells and their verdict
semantics. The three legacy 2026-08-28 attempts are retained for diagnosis,
but none is formal M15 evidence; see
[`evidence/m15-window/REPORT.md`](../crates/agent-eval/evidence/m15-window/REPORT.md).

**Prospective-route amendment, 2026-08-30.** Sections 5 and 7 now distinguish
the base product candidate from an optional settlement-projection candidate.
This does not change the frozen cell shape, evidence contract or verdict in
§§2–4, and it does not reinterpret any prior formal M15 window.

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

That preflight passed on 2026-08-28 and pinned PinAI `/v1`,
`gpt-5.6-luna`, Responses, 128,000 tokens. The source-bound dirty-tree
`retry_policy_dev` normal cell passed behavior, diff and closure-required
completion in 26 rounds / 59 calls with zero provider retries. It is not one
of the formal cells and makes no comparative performance claim; its role is
only to prevent spending the clean 12-cell budget on an unproven serving.

The PinAI serving tuple stayed pinned through the first v4 window, but this
source-bound product preflight predates the catalog-cold `fs.mkdir` addition
and its admission decision.
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

The first formal v4 window is also a valid FAIL:
`crates/agent-eval/evidence/m15-window/_windows/1788093162603` (run
2026-08-30 on the predeclared clean source `d1936d4`, product surface,
pinned serving): 9/12 pass, 0 NOT_RUN, all provider healthy. Migrate 4/4 and
policy 3/4 (normal r2 exhausted the 48-round tool budget before closing);
diag 2/4, its two failures again using the invalid `checked_shl` saturation
strategy against the frozen saturate-not-wrap oracle while the r1 and the
resume r2 of the same pack passed. Per the §5 cross-window rule this valid
FAIL rejects the current base candidate and returns to diagnosis; the window
is not rerun. M15 remains open.

Post-window diagnosis (2026-08-30) under
`crates/agent-eval/evidence/m15-diagnosis/REPORT.md` closes both mechanisms
from the immutable cell streams: the diag failures are an exact
overflow-truncation fault (`checked_shl` checks only the shift count, so
`100u64 << 62` truncates to `0`; the frozen `u128`-widening marker names the
same defect class), and the policy failure is an incomplete closure loop
(completion refused on a stale verification basis plus one unresolved failed
command; the model refreshes the PASS but never re-proposes). No harness,
transport or oracle defect is present; candidate selection is a user
decision.

The serving/model candidate then switched by user decision (2026-08-30) to
the localhost OpenCode relay tuple (`http://127.0.0.1:8787/v1`,
`deepseek-v4-flash`, Responses, 128,000 tokens, 16,384 max output tokens).
Its preflight chain surfaced one harness defect, fixed in `provider-openai`
(commit `a242736`): a model call recorded raw while its tool was not yet
exposed (e.g. `fs_mkdir` for `fs.mkdir`) stayed in history, and the
per-request wire-name codec failed closed once the spec became exposed.
Spec mappings are now authoritative and colliding history wire names are
that tool's wire form (skipped, not errors); spec-vs-spec collisions still
fail closed. The relay bounded preflight then passed 2026-08-30 on commit
`a242736` (clean head; source tree digest `f8b57b46a3e56c49...`): one
`retry_policy_dev` normal cell with the product surface (TaskProgress on,
settlement and advisory candidates off) completed with behavior/diff pass,
closure completed, provider healthy, 23 model rounds / 35 tool calls.
Evidence: `crates/agent-eval/evidence/m15-preflight-relay/`, with the three
earlier failed attempts retained (Chat SSE shape, 4,096-token output
truncation, wire-name collision). The predeclared 12-cell window on this
tuple then ran 2026-08-30 on the clean source `a25a8a5` and is a **valid
FAIL: 10/12 pass, 0 NOT_RUN** — the mechanical report at
`crates/agent-eval/evidence/m15-window/_windows/1788105967425/`. Diag 4/4
and migrate 4/4; policy 2/4. Per the 32,768-window correction below, only
`normal r2`'s explicit output-limit error was cap-bound; `resume r2`'s
~8 KB malformed tool-call argument is the same model wire-quality weakness
that recurs independently of the cap. No harness, transport or oracle
defect; per §5 the valid FAIL rejects the relay candidate and M15 remains
open. Candidate selection is a user decision.

By user decision (2026-08-30) the same relay model was re-pinned with
32,768 max output tokens. A bounded probe established the tuple can honor
the cap: the upstream emitted 22,341 output tokens in one response without
truncation. The new bounded preflight passed 2026-08-30 on clean HEAD
`f32f22d` (source tree digest `16d97ccb81696f8b...`): one
`retry_policy_dev` normal cell with the product surface completed with
closure completed, provider healthy, 17 model rounds / 33 tool calls.
Evidence: `crates/agent-eval/evidence/m15-preflight-relay-32768/`. The
predeclared 12-cell window on this tuple then ran 2026-08-30 on the clean
source `ab4534a` and is a **valid FAIL: 9/12 pass, 0 NOT_RUN** — the
mechanical report at
`crates/agent-eval/evidence/m15-window/_windows/1788109477415/`. Diag 3/4
and migrate 4/4; policy 2/4. All three failures are `malformed-tool-call`
at argument columns far below either output cap (521 / 10,526 / 10,736
characters): the model emitted tool-call argument JSON that ends
prematurely (EOF mid-list) or breaks JSON syntax (`expected ',' or '}'`),
rejected fail-closed by the provider's strict parser. This corrects the
earlier attribution: of the 16,384-window failures only `policy normal r2`
(`model_output_limit`) was cap-bound; the malformed-arguments failures
recur independently of the cap. No harness, transport or oracle defect;
per §5 the valid FAIL rejects the 32,768 relay tuple and M15 remains open.
Candidate selection is a user decision.

By user decision (2026-08-31) the route changed from switching models to
fixing the intermittent malformed tool-call output at the harness. The
relay serves no suitable alternative on the pinned Responses protocol:
only `deepseek-v4-flash`, `deepseek-v4-pro` and `grok-4.5` are available,
the Qwen models (`qwen3.8-max`, `qwen3.7-max`, ...) return 401 "not
supported for format openai", and glm/kimi/hy3/mimo answer 501 (Responses
unavailable). The fix is commit `41f06ad` (CI `33325617880` green): the
default system prompt stated the requirement explicitly — every tool call
argument must be one complete valid JSON value — and
`provider-openai` classifies the model-emitted `MalformedToolCall`
(argument JSON ending prematurely or breaking JSON syntax, at columns far
below the output cap) as retryable, so the eval's buffering transport
re-issues the request from scratch: bounded, never leaking the rejected
stream into the sink, with persistent malformed output still failing
honestly and recording the attempt count. Wire damage (`MalformedEvent`)
stays non-retryable and interactive hosts never replay emitted deltas.
The re-pinned exact-source/product preflight passed 2026-08-31 on clean
HEAD `41f06ad` (source tree digest `d4b4da3517f7a3e8...`): one
`retry_policy_dev` normal cell, closure completed, provider healthy,
16 model rounds / 27 tool calls, evidence
`crates/agent-eval/evidence/m15-preflight-relay-fix/`. The predeclared
12-cell v4 window on this source ran 2026-08-31 on the clean HEAD
`784d7aa` and is a **valid FAIL: 10/12 pass, 0 NOT_RUN** — the
mechanical report at
`crates/agent-eval/evidence/m15-window/_windows/1788115951355/`.
Behavior passed 12/12 and every cell reported a healthy provider; the
malformed-tool-call failure mode did not recur in any cell, and the new
retry path was exercised twice (`retry_migrate_dev resume r2`,
`retry_policy_dev resume r1`), both times ending in a passed cell (one
`model_used` event records 2 attempts / 1 retry). The two failures are
`retry_policy_dev normal r1` and `r2`, both erroring "phase one failed:
tool round budget exhausted after 48 rounds" with `task.complete`
refused 3/3 and 5/5 and no retries — the same cells that failed via
malformed arguments on `ab4534a`. The harness fix removed its target
failure mode and left a model task-execution failure on that fixture;
per §5 the valid FAIL rejects the candidate and M15 remains open.

The uncommitted post-window repair candidate removes that standing JSON
sentence and moves recovery into the protocol: sink-declared replay boundaries,
one immediate format regeneration independent of transport backoff, and typed
attempt incidents which cannot become task completion debt. It also emits a
basis-stamped, single-stage completion repair snapshot. These are development
changes only; they do not reinterpret the `784d7aa` verdict or authorize a new
window. Bounded SSE framing, per-call terminal state and captured-schema
validation remain required before a future source pin.

Post-window diagnosis (2026-08-31,
`crates/agent-eval/evidence/m15-diagnosis-closure-gate/REPORT.md`)
attributes both failures to completion-gate compliance rather than wire
quality: the model delivered the functional task (final workspace
satisfies the directive; `verify.run` green; its 15 tests pass) but its
last trusted verification was made stale by three successful post-verify
`shell.exec` runs, early fail-closed tool-name attempts left permanently
unresolved failed-command rows in the execution ledger, and the model
could not act on the refusal messages, exhausting 48 rounds
(`closure=error`). The passing cells differ behaviorally: tools loaded
before first use, a current `verify.run` as the final action. The
fixture's hidden checks also bind implementation detail (needle text)
and flag three equivalently correct implementations false. Per §5 the
valid FAIL rejects the candidate; M15 remains open.

The later Completion Convergence implementation and report do not justify
another window. The original review found split completion authority,
continuation advancing the directive epoch, pre-success all-criterion coverage
fan-out and replay of an uncommitted suffix. Candidate fixes landed in the
merged audit (`a3bd23f`): host declaration-bound receipts, prospective
  terminal checkpoints and explicit replay barriers. Their local integrated
  matrices are green and Windows CI is green, but the Ubuntu CI exit is not yet
  banked (see `BASELINE-01` in [`AUDIT_TODO.md`](AUDIT_TODO.md)). The experiment also
changed the whole TaskProgress and checked-file GC projection between arms.
Preserve its checker's mechanical FAIL, but classify settlement causality as
`INVALID/CONFOUNDED`. Close the selected-path merged P0 queue in
[`AUDIT_TODO.md`](AUDIT_TODO.md) before spending another formal window.
A settlement-enabled candidate includes the common-prefix causal-fork exit; a
settlement-off base does not.

## 1. V1 candidate composition

| plane | evidence | status |
| --- | --- | --- |
| Platform gates | M12/M13 clean-tree closure-audit artifacts in `evidence/platform-closure/{m12,m13}/` | evidence banked; overall status wording pending `GOV-STATUS-01` |
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
- runtime surface for the current base candidate:
  `project_task_progress=true`, `project_settlement=false`, and other advisory
  candidates off. A future settlement-enabled source requires its own
  prospective promotion and an explicit candidate-record amendment before
  source pin.

Resume interrupts after the first durably settled mutation and its durable
checkpoint. Continuation must restore and continue from the exact acknowledged
`(lineage, task, sequence, artifact, checksum, capability_generation)` tuple.
A source, pack, host-policy, surface or serving change voids the window.

## 3. Evidence contract

The three historical valid-FAIL windows remain immutable
`retry-pilot-cell-v3` evidence. Any prospective candidate uses
`retry-pilot-cell-v4`; this schema advance adds stable pair/source identity,
independent acceptance-domain revision/source identity and bounded
model-request causal-audit fields. It does not reinterpret v3 verdicts.
The four formal v4 windows are banked as valid FAILs (9/12 on `d1936d4`,
10/12 on `a25a8a5`, 9/12 on `ab4534a`, 10/12 on `784d7aa`); no v4 window has passed. Each
immutable cell directory
contains the manifest, full event stream, `dimensions.json`, hidden oracle
records and workspace snapshot hash. The dimensions are persisted facts, not
a projection from the exit branch:

- acceptance profile, verdict and observed oracle result;
- behavior and allowed-diff result;
- provider health plus typed error class;
- restored, exact-tuple-matched and continued as independent booleans;
- turn completion and task closure as independent booleans;
- phase counters and runtime error text, including failure exits;
- completion policy plus the public acceptance domain, declaration revision
  and canonical declaration-source digest;
- exact `project_task_progress` / `project_settlement` values and the assembled
  prompt-layer digest;
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
terminal events, event loss/gaps, missing switch/prompt identity,
summary/dimension drift, escaped cell paths or a dirty tree. Per-cell and
total/max round, tool, wall-time and token facts
come from the persisted event-derived summaries. `agent-eval --m15-report
<window-dir>` rebuilds the same report from persisted facts.
For v4, the reporter requires all 16 identity/switch keys to be present,
including the nullable acceptance-authority triple, and recomputes the frozen
pack, provider, acceptance declaration and exact switch identities before it
accepts the window.

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
2. a materially changed candidate must first pass its deterministic gates and
   an exact-source preflight; if it changes the model-visible settlement
   projection, it must additionally pass an isolated settlement off/on causal
   gate before that preflight;
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

Before spending another 12-cell window, preserve all four valid FAILs and do
not rerun the unchanged JSON-hardened source. A materially new candidate must:

1. pass deterministic attempt-incident versus task-obligation tests, including
   off-surface canonical/wire-name calls which remain visible but create no
   completion debt;
2. return a bounded, basis-bound resolver for every completion blocker and make
   deferred safe-point refusal visible. If proof is the sole blocker, an
   explicit completion intent may run at most one host-declared exact proof-
   refresh transaction under an unchanged world fence;
3. align product and eval format recovery: one independent malformed-tool
   regeneration before any published text, persistent failure fail-closed,
   and typed attempt/recovery metrics;
4. pass a byte-bounded standards-correct SSE framer, strict Responses/Chat
   per-call state machine and immutable-round schema validator before approval;
5. retain the completion/continuation/context/replay/provider matrices and pass
   fmt, Clippy, build, full workspace tests plus Ubuntu and Windows CI on one
   newly recorded clean source;
6. close or prove out-of-path selected P1 items. Only a settlement-enabled
   candidate additionally needs the same-checkpoint off/on causal fork;
7. freeze source, product switches, acceptance identity, surface and serving,
   then pass one bounded exact-source product preflight with explicit protocol;
8. only then predeclare and run at most one uninterrupted 12-cell v4 window.
   Valid FAIL rejects that candidate; only typed `NOT_RUN` permits rerunning the
   whole frozen window.

300×3 scale, `recall_after_fix`, a 27-cell context expansion, a second context
engine comparison and model comparison remain parked until this gate closes.
