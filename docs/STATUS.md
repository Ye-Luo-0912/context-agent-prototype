# Current status

This file answers four questions only. Architecture, lifecycle, execution,
and sandbox contracts live elsewhere. Experiment facts live in
`crates/agent-eval/evidence/*/REPORT.md`. Do not treat
`docs/CONTEXT_RUNTIME_TODO.md` as live contract.

| Doc | Role |
| --- | --- |
| [`AGENTS.md`](../AGENTS.md) | Invariants, no-go, dependency rules |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Stable architecture |
| [`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md) | Context / GC / evidence / retrieval |
| [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) | ResourceFact / freshness / verification / snapshot |
| [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md) | EffectIntent / HostLifecycle / sandbox attestation |
| [`ROADMAP.md`](ROADMAP.md) | Milestone gates and ordered route |
| [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) | Long-task Runtime gaps and development diagnostic |
| [`AUDIT_TODO.md`](AUDIT_TODO.md) | Confirmed defect queue |

## Now

- M10, M11, and M14 are closed at their named gates.
- Context V1 operational core and **Execution Coherence V1 are both
  freeze candidates**: the 2026-08-23 long-flow pass confirmed the
  coherence machinery (MOD-OBS-01 observation, MOD-PROG-01 stall,
  turn checkpointing) held; Warm=Stored rereads stayed 0. Do not retune
  or extend them as product work.
- Provider routing is now explicit and isolated. PinAI is a direct external
  provider using `/v1/responses`; the localhost OpenCode relay is a separate
  base URL and may use its own direct-then-proxy upstream policy. There is no
  PinAI -> localhost or cross-provider fallback. `provider-openai` supports
  bounded Responses SSE/tool continuation plus Chat compatibility, caches only
  same-base unsupported-protocol negotiation, and treats streamed
  `network_error` as a retryable failure instead of an empty completion. This
  improves live-gate validity but is not evidence that the convergence gate is
  closed. The 2026-08-24 post-fix short gates kept the routes separate and both
  passed hidden `add_test`: direct PinAI Responses with `gpt-5.6-luna` used 7
  model rounds / 6 tool calls, while localhost OpenCode `ox-alpha-free` used
  same-base `auto` negotiation and 8 rounds / 7 calls with zero failed tool
  outputs. Both committed their edit on the first attempt. These are transport
  and tool-loop smoke results, not paired long-flow or convergence evidence.
  OpenCode Muse 1.2 remains account-gated by an explicit data-contribution
  opt-in response; proxy fallback correctly does not retry that non-region
  authorization decision. See
  [`provider-routing-smoke-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/provider-routing-smoke-2026-08-24/REPORT.md).
- The subsequent four-cell Tool Edit diagnostic separated editor correctness
  from provider availability. Direct PinAI/Luna passed strict hidden and edit
  gates 4/4 in 15 rounds with zero failed tool outputs; every canonical patch
  committed on its first valid attempt, including CRLF, mixed-EOL, stale-read,
  and two-file cases. Local OX scored strict 2/4 and gate 0/4 initially because
  3 cells hit streamed `network_error`; a bounded route-health/session-rotation
  relay change did not improve a second run because both direct and system-
  proxy paths received the same upstream failure. Do not attribute those OX
  failures to Context or `edit.patch`, and do not use OX for acceptance until
  an independent availability smoke is green. These dirty-tree runs are
  diagnostic only; see
  [`provider-routing-tool-edit-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/provider-routing-tool-edit-2026-08-24/REPORT.md).
- A direct PinAI/Luna Responses `late_constraint_long` A/C diagnostic then
  passed hidden verification in both arms and confirmed that C's Context
  advantage remains large: model input -34%, selected resident/reactivated
  tokens -87%, historical-context tokens -80%, and final resident bytes -98%
  versus A. Execution still amplified to 75/85 rounds/calls versus A's 65/57,
  with 29 versus 9 failed outputs. Twenty-five C failures were malformed
  pagination capabilities. Treating empty/zero cursors as page one was tested
  and rejected: C grew to 137 rounds / 171 calls, max-turn rounds rose 12→47,
  and the paired A cell timed out. A follow-up strict-schema run also failed:
  C passed hidden but used 107 rounds / 112 calls, while A timed out at turn 6;
  the model fabricated regex-shaped artifact identities and all 25
  `fs.list`/`search.grep` calls failed. The retained design exposes only one
  model continuation surface: first-page tools return a bounded view plus
  `artifact_ref`, and `artifact.read` reads further lines; legacy per-tool
  cursors remain parser-only. The same traces exposed a separate
  `context.manage` union-parser defect, now corrected by parsing only fields
  relevant to the selected op while keeping required/relevant values strict;
  its follow-up calls passed 4/4. Context selection, retrieval budgets, GC,
  autonomy, and packing are unchanged. See the retained
  [`baseline report`](../crates/agent-eval/evidence/longflow-pinai-luna-responses-2026-08-24/REPORT.md)
  and the
  [`negative experiment`](../crates/agent-eval/evidence/longflow-pinai-luna-cursor-normalized-2026-08-24/REPORT.md),
  plus the
  [`strict-schema follow-up`](../crates/agent-eval/evidence/longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md).
  That follow-up also isolated five failed `task.complete` calls caused by
  model-echoed artifact claims. Runtime already attaches current assistant and
  verification evidence at completion, so the artifact list is now
  parser-only compatibility and the model supplies only the bounded summary.
  A retained follow-up pair passed 4/4 in both arms with only 2/1 failures;
  C still used 77 rounds / 84 calls versus A's 49 / 36, proving failure
  cleanup alone does not close convergence. See
  [`longflow-pinai-luna-unified-artifacts-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pinai-luna-unified-artifacts-2026-08-24/REPORT.md).
  `task.complete` is now terminal at the same durable safe point when its
  entire sibling batch succeeds and current verification remains valid; a
  failed sibling still gets another model recovery decision. The live trace
  proved the confirmation round disappeared, but also exposed the deeper
  loop: C closed the durable task on 9/15 turns versus A's 3/15, clearing
  task affinity and causing the next directive to rediscover tools/files.
  See
  [`longflow-pinai-luna-terminal-completion-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pinai-luna-terminal-completion-2026-08-24/REPORT.md).
- A task-continuity candidate now separates implicit turn completion from
  durable multi-turn task closure. `task.complete` is catalog-cold during
  ordinary work and is leased by explicit closure intent or a task-owned
  requirement; `capability.manage` discovery remains available. Deterministic
  tests prove ordinary vs explicit surface selection, clean one-shot commit,
  and failed-sibling recovery. A live `add_test` smoke passed in 3 rounds / 2
  calls / 0 failures. Two initial long-flow A/C diagnostics then removed every
  incidental task closure and reduced C to 49/44 and 57/52 rounds/calls,
  versus A's 50/45 and 47/38. Median C model input stayed 24% below A,
  selected tokens 76% below, and final resident bytes 80% below. This is the
  first candidate to bring C execution close to A without expanding Context
  in those two pairs.
  It is **not accepted yet**: C hidden success was 3/4 then 4/4 while A was
  4/4 twice. The miss was a committed `RELEASE.md` update that wrote
  `Version 2` instead of the checker-required literal `v2`, not a missing edit
  or filesystem failure, but it still keeps the success-neutral gate open.
  Do not claim convergence or M15 closed. See the combined
  [`task-continuity report`](../crates/agent-eval/evidence/longflow-pinai-luna-task-continuity-2026-08-24/REPORT.md)
  and the
  [`independent repeat`](../crates/agent-eval/evidence/longflow-pinai-luna-task-continuity-r2-2026-08-24/REPORT.md).
  A later complete pair is a required counterexample: both arms passed 4/4,
  but C regressed to 82 rounds / 76 calls / 415,897 input tokens versus A's
  47 / 36 / 291,034, with a 30-round edit-repair turn. Task completions stayed
  zero, so the old task-affinity loop did not recur. The trace instead showed
  a committed patch that omitted a final module terminator, a prefix-only
  success echo that hid the file tail, and repeated ordinal repairs against
  ambiguous `}` anchors. `edit.patch` now requires a unique exact anchor on
  the model surface (legacy `occurrence` is parser-only) and bounded success
  echoes retain both head and tail. A post-change live `add_test` smoke passed
  in 4 rounds / 3 calls / 0 failures with the first patch committed and no
  confirm read or fallback. That proves interface compatibility, not a causal
  long-flow improvement; convergence remains open. See
  [`post-continuity r3`](../crates/agent-eval/evidence/longflow-post-continuity-r3-2026-08-24/REPORT.md).
  The first post-hardening unchanged-workload pair then passed 4/4 in both
  arms: C recovered to 53 rounds / 51 calls / max-turn 7 versus A's 54 / 44 /
  13, with zero C failed outputs. C input was 37% lower, historical Context
  66% lower, selected tokens 76% lower, and resident bytes 80% lower. No
  ordinal field or task completion appeared in C's trace. C still used seven
  more calls; three were reads of zero-byte successful verification artifacts.
  Process output now omits `artifact_ref` for zero-byte captures while keeping
  non-empty/truncated artifacts unchanged. Do not synthetically subtract those
  calls from the measured result. This is one positive dirty-tree pair, so the
  formal gate remains open pending an independent repeat. See
  [`unique-anchor r4`](../crates/agent-eval/evidence/longflow-post-edit-anchor-r4-2026-08-24/REPORT.md).
  That independent post-output pair also passed 4/4 and eliminated empty
  artifact reads, but did not close convergence: C was 47 rounds / 44 calls /
  max-turn 6 versus A's 43 / 32 / 6. C retained 21% lower model input, 57%
  lower historical Context, 69% lower selected tokens and 76% lower resident
  bytes, while using twelve more calls. The gap was evidence/discovery (29 C
  evidence-only results versus 16 A), concentrated in a targeted already-done
  turn where globally novel Git/catalog facts kept resetting the old global
  Evidence Frontier. Runtime now treats frontier progress as task-relevant
  after an exact Fresh directive target exists: unrooted novel evidence is
  retained but does not clear convergence debt; open-ended directives keep
  broad exploration. Exact current selected bodies co-locate a currentness
  marker, and verification recipe ids are explicitly values of `verify.run`,
  not tool names. Context/GC and autonomy are unchanged. This correction
  postdates the measurement, so a new pair is required. See
  [`task-relevant frontier r5`](../crates/agent-eval/evidence/longflow-post-empty-artifact-r5-2026-08-24/REPORT.md).
  That pair is now a counterexample rather than acceptance. Both arms stayed
  hidden 4/4 and C kept the Context advantage (input -21%, historical -54%,
  selected -65%, resident bytes -72%), but C rose to 57 rounds / 56 calls /
  max-turn 15 versus A's 49 / 38 / 7. The advisory fired but did not stop an
  already-satisfied turn from spending 15 rounds and 16 calls, including six
  repeated `git.status` calls. Exact surface events show the lower-level
  amplifier: loading `git.status` immediately displaced a just-loaded
  `git.diff`, so sequential inspect/load decisions could not assemble a
  cooperating tool set. Runtime now separates pending explicit loads from
  one-decision result delivery. An explicit load remains rooted until exact
  use, unload, or directive end; using one member consumes only that member,
  with no round TTL or Context change. Deterministic cohort and lifecycle
  tests are green, but this correction postdates r6 and still needs an
  unchanged-workload pair. See
  [`task-relevant frontier r6`](../crates/agent-eval/evidence/longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md).
  The r7 pair confirmed the cohort mechanism but rejected it as a sufficient
  convergence fix. Both arms remained hidden 4/4, the old Git churn fell from
  `git.status` 7→1, `git.diff` 2→1 and `capability.manage` 15→10, and max-turn
  recovered 15→8. C still used 62 rounds / 59 calls versus A's 46 / 35.
  Eight of C's ten catalog operations addressed universal coding primitives,
  while the compact `fs.write` + Git schemas cost only about 190 tokens per
  round. The next isolated candidate therefore moves `fs.write`, `git.status`
  and `git.diff` into the stable production core (about 947 total schema
  tokens, under the unchanged 4,096 cap); effect authority and Context are
  unchanged. This candidate postdates r7 and must be reverted unless an
  unchanged pair reduces rounds/calls at hidden parity. See
  [`pending tool-cohort r7`](../crates/agent-eval/evidence/longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md).
  Three unchanged stable-core pairs now support retaining that boundary. r8 was
  C/A 46/46 rounds and 41/37 calls; r9 was 49/47 and 46/38. All six arm-runs
  passed hidden 4/4, `capability.manage` was one call per arm in r8/r9, and C
  kept 22–39% lower input plus 61–67% lower historical Context. This reduces the
  pre-surface r7 gap from +16 rounds/+24 calls to 0/+4 and +2/+8. It does not
  close convergence: r9 C max-turn was 9 versus A's 7 and its Hello edit
  needed three patch calls after two sequential hunks targeted the same anchor.
  `edit.patch` now requires explicit model-visible `replace`, `insert_before`
  or `insert_after`; inserts preserve their unique anchor, while omitted op
  remains parser-only replace compatibility. The unchanged r10 live repeat
  passed hidden 4/4 in both arms at C/A 48/47 rounds, 41/39 calls, identical
  three failed outputs and max-turn 8. C used 39% less model input, 67% less
  historical Context, 78% fewer selected tokens and 84% fewer resident bytes.
  Explicit inserts were exercised; the r9 conflicting-anchor Hello tail did
  not recur. The remaining two C patch refusals were exact-match-safe
  `ambiguous_match` / `no_exact_match` locator errors and recovered without a
  filesystem settlement failure. Across r8-r10 the median gap is +1 round / +4
  calls. Retain the stable core and explicit operations; do not add positional
  or fuzzy edit authority from one ambiguous sample. See
  [`stable core r8`](../crates/agent-eval/evidence/longflow-stable-core-surface-r8-2026-08-24/REPORT.md)
  and
  [`stable core r9`](../crates/agent-eval/evidence/longflow-stable-core-surface-r9-2026-08-24/REPORT.md),
  then
  [`explicit edit operations r10`](../crates/agent-eval/evidence/longflow-explicit-edit-ops-r10-2026-08-24/REPORT.md).
- Evidence-backed coherence correction after the post-resolver longflow:
  TaskProgress identity no longer erases the only selected file body.
  Descriptor pricing requires exact same-request `path@revision` body
  presence; checkpoint restoration selects actual spill demand and spends
  the existing hash-only revalidation quota on those identities first.
  Unknown safety, context/send budgets, GC thresholds, and model autonomy
  are unchanged. Same-result currentness repair is now
  `EvidenceReconfirmed`, which does not clear convergence debt; its dormant
  fingerprint stays bounded and never enters TaskProgress.
  One production-surface live diagnostic passed both hidden arms and moved C
  from the preceding two-cell mean 65.5 rounds / 90 calls to 51 / 64 while
  used-round model input fell 24%; see
  [`longflow-body-coverage-2026-08-23/REPORT.md`](../crates/agent-eval/evidence/longflow-body-coverage-2026-08-23/REPORT.md).
  This is directional `n=1` evidence only: C still exceeded paired A, wall
  time did not improve, and C provider-total tokens are a retry-induced lower
  bound. Context itself retained its advantage: historical tokens -59%,
  selected tokens -71%, resident bytes -71%, and per-round model input -19%
  versus A. Extra rounds inflated TurnFrame/schema enough to leave whole-task
  used-round input +3%; optimize execution amplification without retuning
  Context. Do not claim the residual convergence problem closed.
- The 2026-08-24 execution-amplification audit kept one narrow protocol
  improvement and rejected one prompt-level shortcut. Bounded TurnFrame
  checkpoint receipts (at most six body-free outcome rows) activated in only
  one C round, so they are correctness/observability rather than a convergence
  claim. A cross-turn `TaskProgress.task_changes` projection was tested and
  fully reverted after its refinement amplified C to 127 rounds / 174 calls.
  On the retained-receipt run both hidden arms passed; C still used 52% less
  historical context, 66% fewer selected resident/reactivated tokens, 73%
  fewer resident bytes, and 21% less input per round than A, while extra rounds
  left whole-task input 2% higher. Context stays frozen; the next execution
  candidate must pass deterministic replay plus paired-live long-tail gates.
  A subsequent two-repeat generic "current workspace is authoritative"
  system-policy candidate also failed that gate (C 64/79 and 72/76 versus A
  44/30 and 43/29) and was reverted; it induced repeated completion and
  verification activity across unrelated turns. Event-only reaggregation now
  isolates the retained-run floor: C/A had the same eight Known mutation
  outcomes, while evidence-only results were 48/21, Unknown invalidations 9/0,
  and the maximum outcome-free result streak 18/3. C exposed 134 reported
  catalog-optional rows (118 unused in their round) versus A's 28 (26 unused).
  The complete +36 call gap partitions into +27 evidence-only and +9 Unknown
  results. `agent-eval` now renders/bundles these outcome-shadow and optional-
  surface metrics. A first runtime behavior slice now uses source-driven
  schema leases: exact called tools survive until their result is consumed;
  explicitly loaded but unused tools form a directive-local pending cohort
  until exact use, unload, or turn end. Host/operator loads are a separate
  persistent source until explicit unload; Runtime/model loads never become
  task-global pins, and checkpoints carry residency rather than minting host
  intent. Explicit task and typed need roots stay loaded; catalog reload
  remains available. A bounded
  `ExecutionBatchSettled` ledger now counts transient/refused/reused results
  without entering Context or the prompt. Oversized provider batches execute
  no member but still receive exact no-dispatch terminal accounting. Lease or
  batch audit-write failure now fences before another model decision.
  A second execution-only slice adds fail-closed pre-dispatch tool purpose and
  target attribution, an eight-row revision-bound negative-path fact table
  with live Workspace checks before no-dispatch reuse, typed lifecycle events
  and eval counters, plus exact trusted-verifier source affinity under the
  current task-anchor revision. Dynamic capability roles and output metadata
  cannot self-authorize verification; generic shell/process remain Opaque.
  A third slice adds host-opt-in `ExactCurrentWorld` PASS reuse on the existing
  bounded verification facts. The equivalence tuple is task state + anchor +
  user directive + workspace revision + exact tool/argument digest + a host
  recipe/profile/policy/environment identity digest; raw environment material
  is never stored, and any mismatch executes normally.
  Reuse requires a durable body-free lifecycle event, returns a truthful
  `executed=false` terminal result, and has separate eval counters. A new user
  directive always permits a real rerun. Production now exposes bounded
  `verify.run { recipe_id }` only when the composition root discovers recipes;
  model argv cannot replace host argv and unknown ids have no process
  authority. General project runners remain TaskScoped/Unknown-safe. The
  generic manifest-free Rust test-target compile is the first exact
  source-read-only recipe and binds a complete bounded workspace input
  snapshot, platform, compiler and environment. Transitive sibling modules are
  covered; links/escapes, external-input directives, special files, overflow
  and pre/post identity drift downgrade and execute normally.
  Deterministic contract/state/builtin/dynamic/actor tests pass (including two
  terminal missing-read results and two terminal verification results, each
  from one real dispatch). The fourth real-runtime Convergence Bench scenario
  also proves `verify.run` requested twice produces one spawn, two successful
  terminal results and Recorded/Reused = 1/1. A two-repeat paired live attempt
  on 2026-08-24 is deliberately excluded from performance and success-rate
  claims: all four arm-runs ended on the same retryable provider transport
  failure, with incomplete usage and lower-bound token accounting. The two
  arm-runs that reached `verify.run` both recorded a successful PASS; neither
  requested an equivalent second verification before transport failure, so the
  attempt cannot measure live reuse or C/A convergence. The live gate remains
  open; see
  [`longflow-exact-verification-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-exact-verification-2026-08-24/REPORT.md).
  One independent retry was also excluded: C lost its first provider request in
  both repeats while A failed later, proving severe asymmetric censoring rather
  than an execution comparison. Stop live reruns until the provider is stable;
  see
  [`longflow-exact-verification-rerun-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-exact-verification-rerun-2026-08-24/REPORT.md).
  Broader host-declared equivalence, in-flight joins and obligation-scoped
  provenance sources remain open behind ROADMAP gates.
  Context/GC selection
  is unchanged, so the measured C context advantage remains the baseline to
  preserve.
  Prior retained evidence:
  [`longflow-task-provenance-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-task-provenance-2026-08-24/REPORT.md).
- The first long-task Runtime slice landed (deterministic only): a
  catalog-cold `task.manage` progress proposal applies through the
  existing anchor compare-and-swap at operation-commit time and writes
  the authoritative outcome back into the model-visible result, so a
  stale base revision refuses without touching task state and is
  retryable in the next round. It can update only autonomous fields —
  current interpretation, plan progress, open loops and one replaceable
  `next_action` — so user goal/constraint authority stays structurally
  on the boundary/approval path. Success publishes `TaskAnchorChanged`
  followed by `TaskProgressUpdated` in event-provable order; refusals
  publish only the typed outcome. Deterministic coverage: tool bounds and
  deny-unknown-fields, accepted/stale actor paths, checkpoint round-trip,
  and single-render prompt projection. Conformance surface tables were
  aligned with intent-gated completion (`task.complete`/`task.manage`
  are catalog-cold; the unload path tests a genuinely optional tool).
  The later safe-point, completion and first live-pilot results are summarized
  below; this slice alone made no live claim.
- The second long-task Runtime slice landed (deterministic only): fully
  settled batches accrue bounded checkpoint debt (anchor change, durable
  workspace mutation, verification change); debt installs the bounded
  resume into the existing task record and schedules exactly one atomic
  write under the workspace state directory. `TaskResumeCommitted`
  precedes `CheckpointDurable`, which lands before `TurnCompleted`; a
  failed write re-arms the debt as `CheckpointWriteFailed`. Completion waits
  for in-flight writes. Continuation also waits for settlement but currently
  does not reject a failed-write outcome; that residual is `LONGTASK-04`
  (since resolved by the LT-RUN-04 Slice D watermarks).
  `continue_active_task` starts a fresh turn from the stored current
  directive and resume state with a `task_continuation` input kind — no
  new user instruction, no re-ingest, `TaskContinuationStarted` is
  event-visible. Read-only rounds accrue nothing. Deterministic
  coverage: ordering, no-debt read-only rounds, store atomicity and
  fail-closed locations, and continuation identity. LT-RUN-03 and the first
  `retry_policy_dev` pilot have since run; their remaining proof gaps are
  summarized below and in
  [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).
- The third long-task Runtime slice landed (deterministic only): the
  completion acceptance gate re-runs at every edge — no recovery fence,
  no unsettled cancelled operation, zero open failure obligations,
  required verification current, and no open loops silently erased. A
  gated one-shot proposal returns its decision to the model with one
  warning per turn instead of committing; deferred and `/done`
  completions fail with the typed reason. Successful closure orders
  `TurnCompleted` -> final durable checkpoint acknowledgement ->
  `TaskCompleted`, provable from JSONL; a failed final write surfaces as
  a warning that never un-completes the task nor claims resumability.
  Deterministic coverage: open-loop refusal with later resolution and
  full ordering proof. All three deterministic LT-RUN slices are green; the
  first live `retry_policy_dev` normal/resume cells later ran as summarized
  below.
- The frozen `retry_policy_dev` fixture landed with its deterministic
  layer-1 gate green (2026-08-25, `agent-eval --long-task-gate` plus a
  cargo test running the same gate). One scripted normal/resume pair
  drives two real runtime instances over the production tool surface:
  phase one reads the fixture, records bounded progress through the
  anchor CAS and durably mutates `src/config.rs`; the harness stops the
  runtime, restores a fresh instance across runs through the shared
  durable authority lineage and continues the SAME directive via
  `continue_active_task`; phase two implements the retry policy,
  updates the README/error taxonomy and closes through intent-gated
  `task.complete`. Directive tools are catalog-cold optionals: the
  scripted model leases them via `capability.manage`, matching what any
  live run must do. Acceptance predicates: resume commits > 0, durable
  checkpoints >= 2, continuation and completion events present,
  byte-exact final workspace, empty hidden-check violations, and
  positional ordering `TurnCompleted` -> final durable ->
  `TaskCompleted`. This is a scripted deterministic result, not proof that the
  persisted safe-point artifact can cold-load all Runtime planes. Landing the
  gate exposed a real settlement-order
  defect and fixed it: an accepted `task.manage` advances the authority
  epoch during its own operation commit, so recording the accepted value
  completion afterwards saw a stale epoch and raised a false recovery
  fence that refused later mutations; accepted values now terminalize
  before directive application. The first live C cells (evaluation layer 2)
  are summarized next; layers 3+ remain open behind the provider-stability
  caution.
- The `retry_policy_dev` live pilot executed its first C-engine cells
  (2026-08-25, evidence under
  [`evidence/retry-pilot/REPORT.md`](../crates/agent-eval/evidence/retry-pilot/REPORT.md)):
  the harness exercised semantic interruption on the first durably settled
  mutation, operator-style turn cancel, externally captured checkpoint
  restore through the durable authority lineage and
  `continue_active_task`. This is narrower than cold recovery: phase two
  reuses the same `ContextEngine` object and does not load the actor's
  safe-point artifact from disk. All four canonical cells end with a report
  but no `TaskCompleted`; their event streams contain zero direct
  `task.manage` and zero direct `task.complete` calls, and none of the four
  canonical cells even loaded either tool through `capability.manage` —
  their catalog-control calls fetched shell/process/edit tools instead.
  The evaluator returns on that lifecycle error
  before the post-run cargo check, so the canonical cells do not support the
  earlier “passing coverage” characterization. One retained earlier attempt
  did call `task.complete`, passed the post-run cargo check and failed only on
  the since-fixed Windows diff-path bug. Two additional attempts are retained
  separately as PinAI transport failures. No acceptance claim and nothing was
  retuned. The bounded next task is `LT-RUN-04`: split outcome truth, add a
  harness-owned oracle outside the editable workspace, decouple verification
  basis from progress CAS, offer a positive-evidence-gated one-decision closure
  affordance as a default-off candidate, and make safe-point durability
  independently cold-loadable; see
  [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).
- All four deterministic `LT-RUN-04` slices landed (2026-08-25). Slice A:
  outcome dimensions are recorded independently, read-only acceptance always
  runs on an inspectable workspace, a harness-owned frozen-API oracle executes
  outside the agent-editable workspace after the diff scan, and resume twins
  build a fresh Context engine per phase while crossing the boundary with only
  the checksum-verified artifact locator. Slice B: a progress-only anchor CAS
  advances the record revision and its resume fence without staling a Current
  verifier; only goal/constraint movement marks dependent verification stale,
  implemented as authority gating on the single record revision rather than a
  second counter. Slice C: the advisory `CompletionOpportunity` candidate
  landed behind a default-off host switch — pure eligibility mirrors the
  acceptance gate plus positive durable-work and exact-tuple trusted-pass
  evidence, the body-free key is offered once per basis and persisted bounded
  in `ExecutionState`, the lease prefers `task.complete` for exactly one
  decision with one bounded prompt statement, typed events distinguish
  not_ready/offered/called/ignored/refused/completed, and all eight mandatory
  negatives plus switch-default silence are deterministic-green. Slice D:
  safe-point checkpoints capture every visible plane including the host
  capability registry under the generation handshake, write as sha256 +
  fsync'd atomic-rename envelopes with a corruption-refusing load path, and
  continuation is gated by monotonic watermarks that fail closed until a
  retried write lands. None of this is a live claim: no cell has passed the
  full conjunction, and the CompletionOpportunity promotion gate (Roadmap
  item 8 off/on paired repeats) has not run — the candidate stays off until
  it does.
- The item-8 off/on paired live gate for the `CompletionOpportunity`
  candidate ran twice (2026-08-25, 8 cells each over `retry_policy_dev`,
  C engine, evidence under
  [`evidence/opportunity-gate/REPORT.md`](../crates/agent-eval/evidence/opportunity-gate/REPORT.md))
  and **failed to promote both times; the candidate stays off**. Attempt 1:
  zero offers armed — discovered verifiers are TaskScoped by design, so no
  exact-identity receipt ever existed and the fail-closed precondition never
  held. Attempt 2 registered a host opt-in source-read-only
  ExactCurrentWorld recipe on both arms: receipt-backed offers fired once
  per mode, and one cell executed the full intended chain live (offer ->
  leased `task.complete` call -> committed closure -> behavior/diff/closure/
  continuation all pass). Promotion still fails: paired outcomes fell 2/4 to
  1/4, medians rose, arming stayed rare, and a journal-lock flake censored
  an off cell (exclusive-lock conflict on the workspace effect journal
  during checkpoint artifact load — recorded as a real defect candidate).
  Separately, the deterministic already-satisfied replay froze green: with a
  registered exact recipe, one offer fires per basis and the leased decision
  closes through `task.complete` alone, while the disabled twin stays fully
  silent (`--long-task-gate` now runs this pair).
- **Execution Convergence V1 mechanism landed** (2026-08-23, all 22
  items checked — the checklist is now the historical record
  [`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md)):
  Evidence Frontier + FrontierDelta + `ExecutionFrontier` events and
  eval metrics; RetryDomain (`ExecutableResolution`, no K-strikes);
  per-turn protocol body cache with checkpoint-gated rehydration and
  event-level hit/miss accounting (`ProtocolBodyCacheStats`);
  versioned `HostPolicySnapshot`; unified surface pressure budget;
  replay frontier rebuild + conformance serde contracts. Verification:
  `agent-eval --convergence-bench` four deterministic scenarios PASS
  on the real runtime + real tool surface.
- Clean A/C longflow runs completed 2026-08-23. The post-obligation
  run (n=2, all four arm-runs passed hidden verification): C r1 61
  rounds / C r2 64 rounds, A r1 61 rounds / A r2 47 rounds — first run
  with live cache accounting, which showed hit rate 0 under command
  pressure (every Unknown-footprint command cleared the whole turn
  cache) and guessing chains whose attempts never escalated because any
  same-domain success cleared the obligation. Both findings are fixed:
  the body cache now suspends on Unknown mutations and revives entries
  after BeforeModel revalidation proves the identical digest
  (PROTO-EVID-03); obligations are lineages with precondition epochs and
  fingerprint-matched resolution (CONV-03), event-visible end to end
  (CONV-OBS-01). Facts in
  [`crates/agent-eval/evidence/longflow-post-obligation-2026-08-23/REPORT.md`](../crates/agent-eval/evidence/longflow-post-obligation-2026-08-23/REPORT.md).
  Context GC and compaction policy stay frozen — C carried ~4.7 KB peak
  resident vs A's 231K historical-context tokens at equal rounds; do not
  reopen either from these numbers.
- Trust & Obligation first cut landed (22-item program complete,
  historical record
  [`TRUST_AND_OBLIGATION_TODO.md`](TRUST_AND_OBLIGATION_TODO.md)):
  Evidence Frontier + FrontierDelta + `ExecutionFrontier` events;
  RetryDomain (`ExecutableResolution`, no K-strikes); per-turn protocol
  body cache with checkpoint-gated rehydration and `ProtocolBodyCacheStats`
  accounting; capability-output metadata sanitizing; real
  `ArgumentDigest` evidence identity; versioned `HostPolicySnapshot`;
  unified surface pressure budget; replay frontier rebuild +
  conformance serde contracts. Verification:
  `agent-eval --convergence-bench` four deterministic scenarios PASS
  on the real runtime + real tool surface.
- M12 first cut: structured `EffectIntent` + trusted `HostToolPolicy`,
  multi-file `WorkspaceWriteSet` bounds, and commit-time
  Actual ⊆ Approved (`MOD-AUTH-01`/`02`).
- M13 first cut: `SandboxProfile` vs post-spawn `SandboxCapabilities`.
- PLAT-06 slices 1–2 (lifecycle / cancel-ACK) landed. Multiplexing is
  not in v0.
- Scheduling/reliability fixes landed 2026-08-23 (AUDIT_TODO
  SCHED-01–04): idle-round `BeforeModel` maintenance gate; explicit
  search candidate completeness with bounded residual verification;
  same-class-across-targets `EXECUTION STALL` cluster escalation; and
  the `protocol-checkpoint-body-missing` reread motive instrument.
  The body cache itself is implemented (see above) with counters, so
  cache claims are verifiable from the event stream.
- CORE-11 registry layering landed 2026-08-23: builtin host policies
  moved out of contracts into `tool-runtime`; `agent-compose` owns the
  `HostToolPolicyRegistry` (builtins + fail-closed plugin `admit()`),
  wired into the kernel lease path, approval gate and dispatcher. The
  manifest → operator-review flow is still open (M12).
- Production always-load: `fs.list`, `fs.read`, `fs.write`, `search.grep`,
  `artifact.read`, `edit.patch`, `git.status`, `git.diff`,
  `capability.manage`. Their compact core schemas cost roughly 1k tokens total,
  still below the 4,096-token surface cap. Shell / `edit.replace` /
  `context.manage` / `task.complete` and plugin tools are catalog-only;
  NeedEvidence PreferSurfaces `context.manage`, while explicit task-close
  intent or a task requirement PreferSurfaces `task.complete`.
- Scripted `--compare-arm` still additionally pins `edit.replace` /
  `context.manage`. Do not change that pin.
- Longflow parallel A/C is a separate product diagnostic and now uses the
  production-default tool surface; pair/cell evidence stamps
  `tool_surface=production`. It must not be used to silently change the
  frozen Context Mechanism pin.

**Do not claim M12, M13, or PLAT-06 closed.**

- The typed host-trusted execution-facts channel reached its last behavioral
  consumers on 2026-08-26: context heating and observation identity now read
  `ContextIngress::ToolObservation.facts` (facts-first with per-value legacy
  fallback), the no-attribution verification entry reads its claim from
  dispatcher-lane facts under the same fallback rule, and the attributed
  production path keeps pre-dispatch attribution as the sole reusable-verifier
  authority. Values are identical for every producer class until trusted
  handlers stop stamping metadata keys, so no behavior change is claimed.
  Host-declared verification equivalence classes landed their first slice
  on 2026-08-26 and stay dormant until a host declares coverage domains
  through the recipe table (see
  [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)).
  Obligation-scoped provenance sources also landed 2026-08-26:
  `ExecutionObligation.source_tool_name` is stamped once by
  `record_obligation` from pre-dispatch truth, lease membership derives
  from live ledger rows, and `tool_lease_roots` folds it into runtime
  roots filtered against the catalog. Trusted handlers now stamp native
  typed execution facts at construction time under the reserved
  `_execution_facts` metadata key (sanitizer-stripped from untrusted
  producers), and every builtin family stamps an explicit
  workspace-mutation bound mirroring the temporary name table
  (`process.session` deliberately stays on the fallback pending its bound
  decision); per-handler tests lock native equals derivation. No
  model-visible output shape changed; a live fixture confirmed end-to-end
  behavior. Code comments no longer carry audit tracking ids; the docs own that
  vocabulary.

## Frozen

- GC knobs: `active_threshold` / `archive_threshold` / `gc_max_generation`
  (pinned by `gc_thresholds_are_freeze_pinned`).
- Frozen Context Bench SPEC / pack digest. No 27-cell or 300×3 rerun as
  v0 engineering.
- No embeddings, vector DB, RAG, learned router, Typed EpisodeOutcome,
  or new GC generation algorithm.
- ObservationMemo stays unwired.
- No `MOD-18`. Residual OS isolation is fail-closed for untrusted code,
  not a new slice. WASI is V2.
- Natural-language verify remains the four-needle hint.

## P0 / P1

**P0 — trusted execution.** Finish M12/M13 gates in
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md): one brokerable
`EffectRequest` path. Landed by 2026-08-26: the full admission flow —
an installed package manifest supplies candidate tool names only, the
operator review artifact supplies the actual bindings,
`admit_reviewed` installs them atomically, and versioned snapshots bind
operation authority so an operator update never re-interprets an
in-flight operation — plus per-binding revocation fencing: a lease
stamps its binding's epoch at mint and commit refuses when that binding
was explicitly revoked or replaced since; other tools' in-flight
operations are unaffected, and snapshot identity never fences.
Adding `plugin.foo` policy does not stale an already-approved `fs.write`;
only replacing or revoking the same binding affects that tool's later
authority. Global "revision changed → all old leases invalid" is rejected
by design, and two concerns stay separate: the policy snapshot identity
prevents *reinterpretation* of approved operations, while the explicit
binding revocation epoch is the only mechanism that may fence live
leases — one revision field must not carry both meanings.
Remaining for M12 closure: nothing structural is left on the
reserve/dispatch/ack path. The out-of-process coordinator transport
landed 2026-08-26 as a process-separated durable ledger: `broker_host`
opens the same `ReservationJournal` and serves bounded line-delimited
requests over stdin/stdout, `ProcessEffectBroker` journals each phase
across the pipe and applies effect bodies locally at the requester
(they are not remotable in v0), so crash-window semantics match the
in-process `JournaledEffectBroker` reference exactly — between a
dispatched record and its ack record recovery can only be Ambiguous.
Broker-owned cross-process execution and HTTP/gRPC coordinator shells
stay future work until a consumer actually needs remotable effects;
do not build them speculatively.
Do not build a second registry. Attestation is actual enforced
capabilities; generic process tools stay non-transactional.
UntrustedGenerated stays fail-closed on native. Multi-file
`EffectIntent` and commit-time Actual ⊆ Approved (`MOD-AUTH-01`/`02`)
landed 2026-08-21 — do not reopen them without new authority evidence.
Tool Surface utility scoring stays out of scope until
obligation-scoped convergence has stabilized; do not couple the two
variables again.

**P1 — Context live evidence, not Context retune.** `context-mech.v2`
12-cell A/C live ran 2026-08-21; facts in
[`crates/agent-eval/evidence/context-mech/REPORT.md`](../crates/agent-eval/evidence/context-mech/REPORT.md).
Do not retune GC from it. `add_test` is Tool Surface
(`historical_context=0`), not Context. Engine packs foreground first
(actual tokens). GC-induced reread is `Warm` + `Stored` only.

**P1 — open-turn convergence evidence.** Execution Convergence first
cut landed 2026-08-21: MOD-OBS-01 (a refused mutation is still an
observation), MOD-PROG-01 (stall advisory + deterministic duplicate
refusal), turn checkpointing (`TURN_FRAME_KEEP_EXCHANGES`). The
late-semantic op5 reproduction ran 2026-08-21 (4 live A/C cells under
`crates/agent-eval/evidence/context-mech-convergence/REPORT.md`): the
48-round loop did not recur (r2 C passed in 29 rounds) and the new
machinery fired zero times. Loop persistence is stochastic, but a
2026-08-22 replay proved the edit failure environment was not clean:
all 11 multi-line `no_exact_match` refusals in those four cells were
the deterministic LF-view/CRLF-raw mismatch. Remaining convergence
work is still a deterministic harness where the loop actually forms,
not more Context live cells. Abrupt-loss replay evidence landed
2026-08-26: the agent-replay recovery report now flags tool batches
killed between dispatch and durable settlement with exact per-call
counts, and keeps settle-time missing/unexpected terminals as a live
integrity signal.

**P1 — Tool Surface edit reliability.** `edit.patch` stays the only
production-always-loaded mutation primitive; matching remains exact and no
parallel edit schema was added. The 2026-08-22 implementation now provides:

- LF/CRLF newline-token equivalence with physical-EOL preservation, literal
  lone CR/non-EOL bytes, bounded scans, and a 4 MiB result ceiling;
- one model-visible, revision-required `files[]` schema (the legacy
  single-file form remains parser-only compatibility), a JSON-quoted
  `fs.read` header carrying raw-byte revision/EOL facts, and a complete
  in-order revision manifest outside the bounded edit echo;
- sorted canonical path leases, one pinned bounded snapshot, duplicate-alias
  rejection, short exclusively-created staging names, staged handle/length/
  SHA verification (plus Unix name/inode binding or Windows deny-sharing),
  compare-before-replace, installed-byte verification before and after the
  authority acknowledgement, and preservation of Unix mode bits or the
  Windows readonly bit;
- for Core-managed writes, a synced authority-journal v2 intent before temp
  creation, carrying bounded byte lengths and SHA-256 before/after revisions;
  bounded reopen reconciliation removes only a confined, regular staged file
  whose name identity and complete content are reverified; file writes require
  an existing parent and never create unjournaled directory topology; and
- typed stale/exact-match refusals, bounded topology/candidate output, one
  1200-character multi-file echo that preserves both ends when the middle is
  omitted, explicit model-visible replace/insert intent with unique exact
  anchors (legacy omitted op and ordinal occurrence are parser-only), and
  honest `NotApplied` / `Applied` / `Unknown` settlement.

Unit tests and clippy are green. A post-unique-anchor `add_test` smoke passed in
4 rounds / 3 calls / 0 failures, with the first patch committed and no confirm
read or fallback; it is compatibility evidence, not a long-tail estimate. The
versioned `agent-eval.tool-edit.v2` pack
plus `agent-eval.tool-surface-edit.v3` gate also produced a source-bound r4
dirty-tree diagnostic pass over the current hardened implementation: 4
fixtures × 3 repeats, 12/12 raw-byte truth,
12/12 flow gate, 9/9 non-conflict first patch, 3/3 proactive stale routes,
zero patch refusal/fallback/confirm-read/recovery/unknown, and 42 rounds. Its
wall total was 164,417 ms and reported provider tokens were 258,325; it
preserved all r3 call-quality results while observing lower wall p50/p95. See
the r4 evidence `REPORT.md`. This proves the combined contract on that frozen
surface, not a general task-failure rate or a causal performance gain.

`TOOL-EDIT-02` remains open. Five clean-tree frozen-gate runs completed
2026-08-26 after the hunk `op`-field drift fix (evidence
`tool-surface-edit-v3-clean-tree-2026-08-26*/`, four with REPORT.md).
Strict raw-byte truth passed every applied patch in all five windows —
12/12 in four runs, 11/12 in the first only because one provider session
died before any tool call — so the mutation path is proven byte-perfect.
The gate never exceeded 9/12 and non-conflict-first never exceeded 8/9:
every failure was served-model decision behavior (post-edit confirmation
reads the stale-recovery contract forbids, a stale-revision first attempt,
or one non-exact first-hunk set, each recovered). The diagnostic has
saturated across five windows; the bar stays 12/12 strict, 12/12 gate,
9/9 non-conflict-first on one clean tree, and the item waits for a
materially different provider or model serving.
Deterministic external-race, crash, journal-fault and — since 2026-08-26 —
disk-full coverage are landed: the feature-gated `test-faults` storage seam
injects storage-full refusals at the authority intent, the staged temp bytes
and the committed record, with fixtures pinning nothing-staged, rolled-back
cleanup of a truncated stage, and `Applied { complete: false }` recovery by
hash evidence. Broader staged-byte accounting breadth remains reliability
work; it is not evidence that
the 12-cell diagnostic failed. Clones share the lease and a second official
`Workspace::open` on the same root is refused by the authority-journal lock;
direct or authority-bypassing filesystem writers remain outside it, and
hash→replace is not a filesystem CAS. Typed rollback now confirms cleanup and
terminal journals or returns `RecoveryRequired`; staged/composite rollback
attempts every child with bounded diagnostics, and Core fences later mutation
instead of reporting a plain rejection. Runtime projects preparation-time and
commit-rejection cleanup uncertainty separately as
`execution_cleanup_recovery_required` and
`not_applied_cleanup_recovery_required`, without preserving proposed
revisions as facts. Core-managed prepare crash seams after authority intent,
stage sync, and review record are mapped and recover conservatively. The
trusted context-free prepare entry remains non-crash-recoverable; partial,
substituted, or colliding stage content is retained as `Ambiguous` rather than
deleted. A multi-file effect remains sequential with honest partial recovery.
This V1-candidate gate runs in parallel with — and does not replace or close —
the M12 → M13 mainline.

**P1 — trustworthy long-task closure and cold continuation before broader
agent evaluation.** The r8-r10 stable-core/edit sequence remains frozen: C's
median gap was +1 model round / +4 tool calls while retaining the large Context
advantage. Keep Context, GC and the production tool surface fixed.

`LT-RUN-01..03` and the deterministic `retry_policy_dev` gate are green. The
first four live C cells also ran, but all lack lifecycle closure and the current
evaluator skips post-run acceptance after that failure. Their resume twins prove
operator stop plus restore of an externally captured full checkpoint, not an
independent disk cold start: the Context engine is reused, the actor safe-point
artifact omits the host capability plane, and the artifact store has no load
path, checksum or fsync durability claim. The runtime has since landed all of
those properties (LT-RUN-04 Slice D), but the retained cells predate it, so
they still do not demonstrate cold start.

The next bounded phase is `LT-RUN-04` in
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md): (1) preserve behavioral,
diff, closure, continuation and provider truth independently through a
harness-owned external oracle; (2) retain whole-anchor CAS while giving
verification basis an independent revision meaning; (3) derive a
once-per-basis `CompletionOpportunity` only from current positive terminal
evidence, default-off until its item-8 candidate gate passes;
and (4) produce and cold-load one complete, checksummed,
revision-acknowledged safe-point artifact, with failed durability blocking
continuation. On that final substrate, run retained-C CompletionOpportunity
off/on normal/resume pairs with at least two repeats per mode; require
behavior/outcome non-regression, improved closure, lower median rounds/calls
and no new tail before promotion. Only the promoted frozen setting enters
same-model A/C. After that add diagnosis and multi-file migration tasks.
Criterion origin/authority must land before Completion Proof Ledger shadow
evidence; a model-visible TaskGraph remains evidence-gated. The pilot remains
development evidence, not M15 acceptance, and it does not reorder M12 then
M13.

All four `LT-RUN-04` slices are landed deterministically as of 2026-08-25
(see Now); the live half of this phase remains open: canonical closure cells
under the split-dimension evaluator, and the item-8 off/on paired
CompletionOpportunity promotion gate on the final substrate before any
same-model A/C comparison.

## Next milestone

Engineering mainline is **M12, then M13**, then a V1 candidate, then
formal M15. V2 Self-Iteration stays blocked.

In parallel, complete `LT-RUN-04`, then pass the retained-C default-off
CompletionOpportunity paired gate before same-model A/C. Do not spend another
live pair on the old 15-directive longflow unless a regression specifically
reopens its retained r8-r10 decision.

Context evaluation: `context-mech.v2` 12-cell evidence exists; do not
expand to 27 or 300×3. Live `recall_after_fix` is refused.
`--compare-live-reasonable` is `add_test` only.
