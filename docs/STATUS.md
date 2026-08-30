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

**Implementation checkpoint (2026-08-30, uncommitted worktree based on
`ea8deefc873abee13106de92bbbb3ddbaeb2d423`; not an evidence source):**

- P0 candidate code now derives one `CompletionReadiness`, preserves the
  directive epoch on `TaskContinuation`, mints only post-PASS criterion
  receipts bound to the current host coverage declaration, resolves failures
  by typed identity/domain with fail-closed overflow, and makes required
  Context misses completion-visible. Required-body overlay first proves a
  displacement feasible and only then commits it, so an oversized mandatory
  body records a miss without destroying the useful optional frame.
- Task closure now uses a prospective terminal checkpoint and explicit
  `RuntimeCommitBarrier`. New traces start with a durable format marker, replay
  rebuilds only the committed prefix, and a validated terminal checkpoint is
  stronger truth in the checkpoint-to-audit crash window.
- Runtime startup is one-shot: only a successfully flushed `RunStarted +
  RuntimeCommitBarrier(RunStart)` batch enters `Serving`; a partial append or
  flush failure enters `StartFailed`, rejects later mutation/retry and writes no
  synthetic shutdown completion.
- Provider parsing now fails closed on malformed SSE/known events/tool
  arguments and missing terminal markers. Buffered eval retry is bounded; a
  live sink never replays already published output.
- Eval now has independent task-progress/settlement switches, stable pair
  identity, bounded real-order episode records and a harness-verified
  same-state request audit. The product default path performs neither the
  counterfactual second input nor its request hashing.
- Live settlement causality is a conditional gap: if the selected candidate
  enables settlement, both arms must fork from one pre-exposure durable
  checkpoint and byte-identical workspace while preserving opaque ids and an
  explicitly pinned provider protocol. A settlement-off base skips this live
  pair. The historical
  convergence bundle remains mechanical FAIL and causally
  **INVALID/CONFOUNDED**; it is not reinterpreted.
- The final uncommitted tree passed the four local commands on 2026-08-30:
  fmt check, all-target/all-feature Clippy with warnings denied, all-target
  build, and the complete workspace all-target suite. `BASELINE-01` remains
  open because this is not a recorded clean source and neither Ubuntu nor
  Windows CI has banked the same source.
- M15 remains open. Its shape remains 3 fixtures × normal/resume × 2 repeats =
  12 cells. Historical v3 FAIL windows remain immutable; no v4 formal evidence
  has been run. M12/M13 artifacts remain banked, while `GOV-STATUS-01` still
  forbids a new closure claim or Self-Iteration transition.

### Historical evidence chronology (non-authoritative)

The dated observations below remain useful evidence but do not override the
snapshot, merged TODO or ordered route above.

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
- A historical task-continuity candidate separated implicit turn completion
  from durable multi-turn task closure. At that experiment's surface revision,
  `task.complete` was catalog-cold during ordinary work and leased by explicit
  closure intent or a task-owned requirement; production v5 now always loads
  it as recorded below. `capability.manage` discovery remained available. Deterministic
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
  does not reject a failed-write outcome; the later watermark implementation
  landed, but the 2026-08-27 review reopened `LONGTASK-04` because those
  watermarks alias task-anchor revision rather than snapshot identity.
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
  completions fail with the typed reason. The intended successful order is
  `TurnCompleted` -> final durable checkpoint acknowledgement ->
  `TaskCompleted`, and JSONL proves that order. The 2026-08-27 review found
  that the acknowledged final snapshot can contain inconsistent task authority
  and fail restore validation, so order alone is not completion-durability
  proof. The current failed-write warning occurs after task authority was
  cleared and cannot make it active again; `LT-RUN-05` replaces that behavior
  with two-phase completion that stays pending/retryable until durable ack.
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
  are summarized next; layers 3+ remain open behind the `LT-RUN-05`
  correctness gate.
- The 2026-08-27 post-landing review supersedes the earlier `LT-RUN-04`
  completion wording. The four implementation slices and their deterministic
  suites are present, but Slice B, Slice D and the live evaluator are reopened
  for correctness:

    - the final completion checkpoint may be acknowledged after active-task
      authority has been cleared but before the Runtime task identity is
      cleared, producing a durable artifact that restore validation rejects;
    - safe-point required/durable watermarks alias task-anchor revision although
      workspace and verification state may change without advancing it, so an
      older checkpoint may satisfy a newer basis;
    - automatic capture does not use the external path's stable capability-
      generation handshake, and checkpoint read/retention is not yet bounded;
    - the live harness accepts any later `CheckpointDurable` instead of matching
      the latest committed snapshot by sequence, checksum, artifact and
      capability generation;
    - progress-only anchor CAS is interpreted differently by completion
      acceptance, exact verification reuse and `CompletionOpportunity`; and
    - oracle setup/start failures, runtime/provider failures and failed-resume
      accounting are not yet classified strongly enough for a decision-grade
      result.

  Landed since that review (2026-08-27), the first LT-RUN-05 work package:
  actor-owned monotonic snapshot sequences replace the anchor-aliased
  durability watermarks; acknowledgements retire exactly their artifact's
  frozen debt set; continuation requires no outstanding debt, no in-flight or
  failed write, and a landed sequence; the allocator watermark rides the
  checkpoint lineage without regressing on restore. Runtime suites and the
  scripted normal/resume gate are green with the fence active. Work package
  two landed on top: safe-point, instance and terminal capture share one
  generation-handshaked assembler that validates before persisting; terminal
  completion is two-phase (durable prospective-terminal ack before any
  in-memory commit or TaskCompleted; failed writes leave the task
  pending/retryable); and the store enforces header/payload/artifact byte
  caps plus bounded newest-window retention.

  Earlier raw artifacts remain retained as diagnostic evidence, but their
  pass/fail ratios and medians must not be used for promotion.
- The `CompletionOpportunity` off/on gate did run twice on 2026-08-25
  ([`evidence/opportunity-gate/REPORT.md`](../crates/agent-eval/evidence/opportunity-gate/REPORT.md));
  this replaces the stale statement that it had not run. Attempt 2 proves one
  live offer -> lease -> explicit `task.complete` -> committed closure chain,
  and the deterministic already-satisfied replay remains green (now also
  observing the offer's checkpoint-debt diagnostic). It does not
  prove product benefit or safe promotion: the candidate remains default-off.
  The 2026-08-27 prerequisites for another live window have since landed:
  EXEC-REV-01's independent verification basis, EVAL-05's durable-tuple
  resume correlation and EVAL-06's typed oracle setup classification are
  all fixed with deterministic coverage, and on 2026-08-28 the cold-resume
  matrix itself landed: the scripted gate's resume phase consumes only the
  acknowledged artifact tuple through the verified cold-load path, the
  terminal artifact restores into a third fresh instance with the completed
  task plane visible, capability generation rides capture and every
  acknowledgement, and retention gained an aggregate byte budget.
  `task.complete` joined the always-loaded production surface (surface rev
  v5) as a product choice; the completion acceptance gate remains the sole
  closure authority and refuses premature or unverified proposals. The
  three same-day M15 attempts do **not** validate that surface choice or a
  serving choice. Their v2 bundles projected missing closure as Runtime
  failure despite M15's report-only closure contract, stamped every pack
  with the retry-policy identity/digest, inferred provider health from error
  text, and were summarized by hand with inconsistent arithmetic. The relay
  attempt's six `max_output_tokens` results are model-output-limit failures,
  not proven transport outages. All three attempts are now forensic-only in
  `evidence/m15-window/REPORT.md`; their ratios and apparent deltas cannot be
  used for promotion or causality.

  The historical evaluator repair landed as `retry-pilot-cell-v3`: actual pack
  identity/digest, acceptance-profile-aware verdicts, typed Runtime/provider/
  model/harness failure classes, independently persisted restore/exact-tuple/
  continuation/turn/task facts, and an exact window manifest whose report is
  regenerated from the 12 immutable cell directories. Prospective evidence now
  uses `retry-pilot-cell-v4`, adding stable pair/source identity, independent
  acceptance-declaration revision/source identity and bounded request-audit
  facts. Its reporter requires all 16 identity/switch keys and recomputes the
  frozen identities. Provider transport or harness failure yields NOT_RUN and
  censors a window; `max_output_tokens` yields a model-output-limit cell FAIL.
  Formal execution rejects dirty source, pack/repeat drift and protocol `auto`.
  M15 remains open until the current exact candidate passes its deterministic,
  clean-source/CI, product-preflight and single-serving v4 window gates.

  The 2026-08-28 bounded representative preflight pinned PinAI `/v1`,
  `gpt-5.6-luna`, Responses protocol and a 128,000-token context window for its
  source-bound dirty-tree diagnostic cell
  (`retry_policy_dev`, normal, closure-required) passed behavior, diff and
  committed closure in 26 rounds / 59 tool calls / 3 failed outputs /
  315,468 ms, with zero provider retries and a contiguous observed event
  suffix. It is historical serving-selection evidence only, not a formal M15
  cell, a current-source pin or a failure-rate estimate. The next exact-source
  preflight must pin its own unchanged tuple. An earlier preflight on the same
  serving failed
  closure after 30 rounds / 53 calls / 7 failed outputs; comparing two
  stochastic cells cannot establish a causal round/call improvement.

  The passing cell also localizes cost away from Context selection:
  cumulative historical-context prompt cost was 8,146 tokens versus 119,912
  TurnFrame tokens (model input 324,783; output 10,531). One of the three
  failed outputs was `fs.write` refusing a missing `tests/` parent; recovery
  then consumed three model decisions to load `shell.exec`, create the
  directory and retry the write. Preserve `fs.write`'s existing-parent
  transaction boundary. `TOOL-DIR-01` has now landed deterministically as
  `fs.mkdir`: one exact final component, existing immediate parent, pinned
  handle, authority-v3 Prepared/committed object identity, exact-empty
  rollback and conservative reopen recovery. `fs.write` now names this typed
  recovery path. The tool stays catalog-cold; the `TOOL-DIR-SURFACE-01`
  deterministic admission gate landed (2026-08-28): a failing mutating
  result whose typed metadata names the first creatable directory surfaces
  exactly `fs.mkdir` with `RecoverySurface` provenance for one decision —
  exact-tool provenance, one-decision source lifetime, approval unchanged
  (PreferSurface demand only; a read-only gate still refuses the
  recovery-marked write without dispatch), and no surface change for
  unrelated missing reads. The candidate ships behind a host switch
  (default off). The full 24-cell isolated live paired run completed the same
  day (`crates/agent-eval/evidence/recovery-surface-gate/REPORT.md`), but a
  post-run audit found zero `RecoverySurface`/`next_directory` exposure in all
  24 event streams; all eight policy cells catalog-loaded and successfully
  called `fs.mkdir`. Its off/on differences therefore cannot be attributed to
  the candidate. Status is `NOT_EXERCISED / no promotion`: retain the
  catalog-cold baseline and keep the switch off conservatively, but do not
  advance the always-ready fallback or claim the candidate caused the 55-round
  tail. The diagnosis failure is also evaluator calibration: the checked-in
  golden solution fails its own saturation oracle and fixture self-check never
  runs that oracle. Calibrated 2026-08-29 (fixture authoring, frozen
  task/oracle meaning): the diag golden saturates via `u128` widening, the
  directive and `DIAGNOSIS` name the saturate-not-wrap edge, the hidden check
  demands an overflow-safe marker, and fixture self-check runs each M15 pack
  oracle offline against seed and scripted solution; diag digest regenerated
  to `2fff5157…eeb`, migrate digest unchanged. The evaluator-validity part of
  the pre-window checklist is done, and the one-cell product preflight on the
  observation-foundation source is cleared: `retry_diag_dev` normal
  PASSed 2026-08-29 on the same pinned serving at clean HEAD `09cce69`
  (with the same frozen diag digest) in 14 rounds / 22 tool calls /
  1 failed output / 139,886 ms — zero provider retries, contiguous events,
  6 durable checkpoints, hidden oracle green, and settlement exposed (`seen`,
  pre 9/15 → post 5/7) with ordinary-final closure, no `task.complete`, no
  auto-close. The only unmatched diagnosis marker is the `backoff.rs`
  overflow-safe needle: the written `exponent >= u64::BITS` + `checked_mul`
  + saturation shape beats the oracle but not the reference
  `u128`/`leading_zeros` needle text, a needle-shape miss, not a functional
  failure. The calibrated diag fixture is solvable on the pinned serving; the
  earlier 2-cell `--diag-smoke` failure was the model not solving the
  overflow edge, which the calibration's needle and oracle now reject
  consistently. The same one-cell preflight then passed the resume arm the
  same day at clean HEAD `65f6cc8`: two resumed turns (5 + 4 rounds) /
  19 tool calls / 0 failed outputs / 104,516 ms, hidden oracle green,
  settlement exposed (pre 8/19 → post 1/0) with ordinary-final closure, and
  the same single needle-shape marker miss. Both arms of the one-cell
  product preflight are therefore cleared on the frozen fixture. The first
  formal clean-tree v3 window ran 2026-08-29 on that pinned serving at clean
  HEAD `16ba7c4` (protocol pinned `responses`, 12 cells, 0 NOT_RUN): 11/12
  PASS; the single failure is `retry_diag_dev` resume r2 (six+five resumed
  rounds / 25 tools / 1 failed output, hidden oracle not satisfied on the
  overflow edge — the same `backoff.rs` needle miss plus one failed
  `edit.patch`). Every other cell passes behavior, diff and, where resumed,
  exact-tuple restored-and-continued; closures are 8/12 `task.complete` and
  4/12 ordinary-final, reported not gated. Efficiency facts (mechanical
  report,
  [`evidence/m15-window/_windows/1787966622822/REPORT.md`](../crates/agent-eval/evidence/m15-window/_windows/1787966622822/REPORT.md)):
  rounds total/max 137/21, tools total/max 332/49, wall max 712,990 ms,
  provider input/output tokens 1,408,538/59,419 (lower bounds where a resume
  cell's usage is incomplete). The diag overflow edge is the one recurring
  failure surface across preflight and the window, consistent with its
  calibrated difficulty. M15 remains open: the frozen §4 verdict passes the
  development plane only when all 12 cells pass, so this window is a valid
  failed result. A second clean-tree v3
  window ran the same day at clean HEAD `f625d39` (protocol pinned
  `responses`; cached input now metered, `cached 152,576 / input 1,943,439`
  across the window): 9/12 PASS — all three failures are diag cells
  (normal r1, normal r2, resume r1; resume r2 PASS), while
  `retry_migrate_dev` and `retry_policy_dev` pass all 8 cells with
  exact-tuple restored-and-continued everywhere and 0 NOT_RUN. Across both
  windows, the diag overflow edge is the only recurring failure surface
  (first window 3/4 diag PASS, second 1/4), consistent with its calibrated
  difficulty and with a stochastic per-cell solve rate; the fixture,
  oracle and serving stayed unchanged. A third clean-tree v3 window ran the
  same day at clean
  HEAD `779604559f682dddc54018e99e5fb35b0080e965` (same pinned tuple;
  cached input 200,704 / input 1,942,278): 10/12 PASS — the two failures are
  again diag cells (normal r2, resume r1), and `retry_migrate_dev` +
  `retry_policy_dev` have now
  passed 24/24 cells across all three windows with exact-tuple
  continuation everywhere; 0 NOT_RUN. Diag-cause analysis over the three
  formal windows is fully attributed: every failing diag cell (6/6)
  finalizes the fix with
  `checked_shl(exp).unwrap_or(u64::MAX)` — which only guards shift counts
  ≥ 64, not bits shifted out (`100u64.checked_shl(62)` is `Some(0)`) —
  while every formal-window passing diag cell (6/6) uses
  `checked_mul`/`saturating_mul` (or
  `min(63)` + `checked_mul`); failing cells also self-test with `base = 1`
  configurations that avoid the trap, so their own tests pass while the
  oracle fails. The calibration makes the needle and oracle agree against
  the trap. Formal diag is 6/12 PASS (50%); all same-fixture calibrated
  `diag-smoke` plus formal evidence is 9/17 PASS (52.9%). The recurrence is
  not a harness or transport defect: it is a model/solver limitation on the
  pinned serving. M15 remains open with the diag overflow edge the sole
  recurring failure surface. Three valid failed windows are already evidence;
  a fourth unchanged retry is prohibited until the cross-window decision rule
  in `M15_ACCEPTANCE.md` is frozen prospectively.
- **Completion Convergence observation foundation landed; task-aware control
  plane remains open** (2026-08-29, CONV-CLOSE-01 reviewed): evaluator
  cleanliness now aligns model-visible
  workspace with the allowed-diff policy (`.gate/`, `target/` and
  `Cargo.lock` are gitignored by fixture self-check, so build artifacts
  cannot manufacture cleanup loops the evaluator silently discards);
  event-derived metrics aggregate the first execution-local settlement label;
  dynamic states `Working -> VerificationDue -> VerifiedCurrent ->
  SettledCandidate` derive from the bounded `TaskRecord.resume: ExecutionState`
  and publish label-on-change `ExecutionFrontier` events. Seven deterministic
  actor scenarios are green: ordinary
  final, durable closure, genuine remaining work, mutation after
  verification, stale verification, proposal settlement across
  cancel/resume, and cold restore. The stale runtime comment describing
  `task.complete` as catalog-cold was removed (the v5 registry always loads
  it). Post-review, this is observation evidence only.
  `TaskProgressView.settlement` is populated but
  `PromptAssembler::render_task_progress` does not render it, so the model
  never saw the claimed settlement one-liner. Eligibility also consults only
  verification validity and the execution-obligation ledger; it does not bind
  current user/task authority, acceptance coverage,
  `TaskAnchor.open_loops`/`next_action`, or `failed_commands`. Therefore the
  current `SettledCandidate` means only “execution state currently verified,”
  not “whole task ready to finish.” The live runner (`--conv-gate`,
  [`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md))
  ran 4/4 normal/resume cells PASS with 4/4 event exposure, but it has no
  projection-off control arm; normal versus resume is not an off/on pair.
  Model-chosen `task.complete` therefore cannot be attributed to settlement.
  `--conv-tail` also counts every event after the first candidate even after a
  later mutation reopens work, so it is not a causal efficiency metric.
  CONV-CLOSE-02 must correct task-aware eligibility, wire the projection only
  behind a default-off switch, replace lifetime tails with settlement episodes,
  and then run a real switch-off/on paired gate. Do not claim convergence or
  M15 closed from the current report.
- CONV-CLOSE-02 landed its four delivery steps the same day and ran the real
  switch-off/on paired gate (approved 8-cell budget, `--allow-dirty`):
  task-aware settle (fail-closed at `VerifiedCurrent` without declared
  acceptance coverage), the neutral fact behind the default-off
  `project_progress` switch with request-level tests, settlement-episode
  counters, and `evaluate_conv_gate` per-pair parity. Live cells required
  three bounded repairs first: trusted PASS clears identity-exact
  `failed_commands` on the current basis/directive/workspace tuple,
  request-level plus whole-cell provider retry, and live acceptance
  declaration bound by the trusted verification pass at observation time.
  The gate ran 8/8 cells PASS with 0 NOT_RUN but FAILED promotion: pair-0
  (normal r1) exposure asymmetry (off none / on seen; the off cell recorded
  no trusted verification pass because its model used only the TaskScoped
  `rust.workspace` runner — no exact identity, so the join never armed and
  the cell is inconclusive by rule, while exposed cells used the host
  `jobrunner.exact` recipe whose pass arms synchronously), marker-violation
  parity in 3/4 pairs (needle-shape misses the behavioral oracle
  tolerates), and episode-rounds/calls medians 1→1 (not strictly lower).
  Projection rendering was real and arm-separated: off 0 tokens every
  round, on 430–512 tokens once a candidate existed. Per the frozen rule
  the projection stays default-off and the gate returns to observation; do
  not claim convergence or M15 closed. See
  [`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md).

  The
  frozen CompletionOpportunity off/on paired live gate then ran
  decision-grade on 2026-08-28 (8 cells): it FAILED promotion — the off
  baseline closed a normal cell by itself while no on-cell improved closure
  (one offer armed, its lease was not called) — so per the frozen rule the
  candidate ENDS default-off. `retry_policy_dev` behavior and diff
  dimensions passed in all eight cells; the truth chain held under live
  load throughout.
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
  manifest → operator-review → atomic `admit_reviewed`/revocation flow and
  per-binding epoch fence landed by 2026-08-26. M12 remains open only for the
  bounded production-path closure audit below.
- Production always-load (surface rev v5, 2026-08-28): `fs.list`, `fs.read`,
  `fs.write`, `search.grep`, `artifact.read`, `edit.patch`, `git.status`,
  `git.diff`, `task.complete`, `capability.manage`. `task.complete` closure
  execution stays intent-gated by the completion acceptance gate. Their compact core schemas cost roughly 1k tokens total,
  still below the 4,096-token surface cap. Shell / `edit.replace` /
  `context.manage` and plugin tools are catalog-only; NeedEvidence
  PreferSurfaces `context.manage`.
- Scripted `--compare-arm` still additionally pins `edit.replace` /
  `context.manage`. Do not change that pin.
- Longflow parallel A/C is a separate product diagnostic and now uses the
  production-default tool surface; pair/cell evidence stamps
  `tool_surface=production`. It must not be used to silently change the
  frozen Context Mechanism pin.

**Historical status recorded 2026-08-27:** M12 and M13 were marked closed at
their named clean-tree gates (`platform-closure/m12/` and `/m13/` evidence
reports). Current authority wording is pending `GOV-STATUS-01`. **Do not claim
PLAT-06 closed**: slice 1–2 are landed and multiplexing stays out of v0.

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
  behavior. The repository still contains legacy tracking ids in code comments;
  removing them is a bounded hygiene follow-up required by `AGENTS.md`, not an
  execution-semantics change. New comments keep tracking vocabulary in docs.

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

## P0 / P1 — historical landed chronology

The current pre-M15 queue is the merged audit linked in **Now**. This section is
retained as dated implementation/evidence chronology; its older milestone
labels do not override `GOV-STATUS-01`, and non-conflicting residual backlog
remains in `AUDIT_TODO.md`.

**P0 — trusted execution closure-audit evidence (banked 2026-08-27).** The
gate history below is retained as context. Landed by 2026-08-26: the full
admission flow —
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
M12 is now a closure audit, not an unbounded implementation queue. Nothing
structural is left on the reserve/dispatch/ack path. The out-of-process
coordinator transport landed 2026-08-26 as a process-separated durable ledger:
`broker_host` opens the same `ReservationJournal`, while
`ProcessEffectBroker` journals each phase across the pipe and applies effect
bodies locally at the requester. Close M12 only when one bounded evidence table
shows every brokerable production effect crosses that path, crash windows
reconcile honestly, and authority/revocation fencing holds; generic
shell/process remain named non-transactional exceptions. Broker-owned remote
execution and HTTP/gRPC shells are not V1 requirements without a remotable
consumer.
Do not build a second registry. Attestation is actual enforced
capabilities; generic process tools stay non-transactional.
The bounded closure-audit evidence generator landed 2026-08-27: the
deterministic `agent-eval --platform-closure-m12` run (also a cargo test)
wrote its first PASS report — 28 resolved rows, zero unresolved — under
`crates/agent-eval/evidence/platform-closure/m12/`, covering every brokerable
family on the journaled reserve/dispatch/ack path, NotApplied/Applied/Ambiguous
crash reconciliation through journal reopen, per-binding epoch fencing,
generic-process exceptions executing against an empty journal, and two
independent out-of-process coordinator sessions sharing one durable ledger.
The M13 counterpart landed the same day: `agent-eval --platform-closure-m13`
(real child spawns, per-profile `required ⊆ actual` activation, both refusal
cases, mechanism-proof attestations) wrote its first PASS report — 8 rows,
zero unresolved — under `crates/agent-eval/evidence/platform-closure/m13/`.
  Both gates were then recorded as closed 2026-08-27 on clean-tree regeneration
  of the two reports (commit-bound source digests in each manifest); current
  closure wording remains governed by `GOV-STATUS-01`.
M13 is likewise a closure audit: structured attestation must validate enforced
evidence, activation must enforce `required ⊆ actual`, and unsupported native
`UntrustedGenerated` must fail closed. Universal native availability belongs
to the WASI/V2 candidate, not the V1 gate. Multi-file
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

`TOOL-EDIT-02` is no longer waiting for another unchanged provider/model
window. On the v4 surface, both archived clean-tree windows reached strict
12/12 with zero confirmation reads; each scored gate 11/12 and
non-conflict-first 8/9. A third console-only window reached the full bar but is
not archival evidence. The only repeated archived miss is a byte-perfect,
revision-correct two-file patch whose hunk partition differs from the hidden
`exact_hunks` decomposition. These finite results support the frozen surface;
they do not prove a general editor-engine failure rate.

The product route is byte/revision/settlement truth: hunk partition is not
model-visible authority and no downstream consumer currently requires a golden
decomposition. `exact_hunks` is now versioned to accept byte-equivalent
decompositions while preserving submitted paths, strict final bytes, revision
discipline, atomic settlement and no-fallback/no-confirm-read checks; the gate
is `agent-eval.tool-surface-edit.v4`. Reverse that choice only if a real
consumer first documents a canonical-granularity requirement. One archival 4x3
confirmation window on the versioned gate is now landed:
`tool-surface-edit-v4-clean-tree-2026-08-26-r4` scored `strict 12/12 gate
12/12 non_conflict_first 9/9` with zero confirmation reads; do not spend more
live windows on the unchanged ambiguous contract.
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

**P1 — long-task recovery integrity before further live promotion.** The
r8-r10 stable-core/edit sequence remains frozen: C's median gap was +1 model
round / +4 tool calls while retaining the large Context advantage. The current
audit does not weaken that Context finding; it invalidates later recovery and
evaluation claims. Keep Context selection, GC, prompt packing and the stable
tool surface fixed.

`LT-RUN-05` in [`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) repaired
and re-proved the existing `LT-RUN-04` substrate rather than adding a new
planning algorithm:

1. introduce an actor-owned monotonic snapshot sequence independent of
   `TaskAnchor`; continuation requires no debt, no failed or in-flight write,
   and `durable_sequence >= required_sequence`;
2. make completion two-phase: validate and durably acknowledge a prospective
   internally valid terminal snapshot before committing in-memory completion;
   share the stable capability-generation capture path;
3. bind cancellation, restore and continuation to the exact durable-lineage/
   task/sequence/artifact/checksum/capability-generation tuple;
4. define one verification basis and currentness predicate shared by
   completion, exact reuse and `CompletionOpportunity`;
5. classify oracle setup, provider, Runtime, behavior, diff, closure, restore
   and continuation independently; require every mandatory dimension and no
   runtime error for PASS, and preserve failed-path round/call accounting; and
6. prove same-anchor multi-snapshot order, out-of-order ack, failed-write retry,
   final-artifact restore, stale capability generation and progress-only
   verification movement with deterministic tests.

The deterministic snapshot/cold-restore chain and evaluator reconstruction are
green. The retained-C CompletionOpportunity off/on gate then ran eight cells
and failed promotion, so that candidate has ended default-off; do not spend
another pair on it. The newer 55-round / 129-call tail established Completion
Convergence as the pre-M15 readiness task. `task.complete` was always visible
and its 18 calls in the 24-cell run all returned successful tool results; the
tail made no completion call. The first implementation landed useful
observation labels, events and tests, but review found that the label is not
rendered to the model, its eligibility is execution-local rather than
task-aware, its tail metric does not stop when work reopens, and its live runner
  has no off/on treatment arm. That historical judgment assigned the work to
  CONV-CLOSE-02; the 2026-08-30 merged audit supersedes it. Do not auto-close,
  resurrect CompletionOpportunity, add fixed stopping counts, or change
  Context/GC. Same-model A/C and broader diagnosis/multi-file twins remain after
  formal M15. Full CPL and model-visible TaskGraph research stay deferred;
  bounded criterion receipts are current work under `ACCEPT-RECEIPT-01`.

## Next milestone

The next milestone is a **recorded and CI-proven V1 candidate**, not another
live sample. Execute in this order:

1. freeze the candidate as one recorded clean source and repeat the four local
   commands plus Ubuntu and Windows CI on that exact source;
2. measure `VERIFY-ROUTE-01`, then close or prove out-of-path every P1 item
   exercised by the selected candidate/evidence path;
3. only if the selected candidate changes model-visible settlement, fork both
   causal arms from one pre-exposure checkpoint/workspace and pin an explicit
   provider protocol;
4. run the bounded exact-source product preflight with the selected serving
   tuple and explicit protocol;
5. if every gate remains green, spend exactly one predeclared M15 window.

A base candidate with settlement projection off may proceed after these gates.
Only a candidate that changes the model-visible settlement projection needs a
new isolated off/on promotion gate. A valid formal failure rejects that source;
only a typed `NOT_RUN` permits rerunning the whole frozen window.

Post-M15 `LT-EVAL-06` development twins remain parked. The deterministic
`harness_maint_dev` fixture is available, but no TaskGraph, learned planner,
Context/GC retune, 27-cell expansion or 300×3 run is authorized. Self-Iteration
remains blocked by the governing milestone gates.
