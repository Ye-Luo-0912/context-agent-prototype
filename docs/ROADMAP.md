# Prototype Roadmap

Current milestone authority. Dates live in git history, not in this
header. A milestone is complete only when its named acceptance holds,
not merely when one implementation path exists.

Design: [`ARCHITECTURE.md`](ARCHITECTURE.md),
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md),
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md),
[`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md).
Now/freeze: [`STATUS.md`](STATUS.md).
Defects: [`AUDIT_TODO.md`](AUDIT_TODO.md).

Historical P0–M11 landing notes are in git history of this file. Do not
copy them back.

## Current gates

| Milestone | Status | Gate |
| --- | --- | --- |
| M10 Runtime Consistency | ✅ | Runtime and context never split-brain on task/restore/turn commit. |
| M11 Context Recall | ✅ narrow retrieval | Search/inspect/fetch without polluting prompt history. Broader catalog work is not a reason to reopen recall. |
| M12 Effect Runtime | 🟡 first cut | One `EffectRequest`/commit path for brokerable effects. Structured `EffectIntent` + `HostToolPolicy` landed. Generic shell/process stay non-transactional. HTTP broker still needs reserved/dispatch/ack. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). **Not closed.** |
| M13 Extension Sandbox | 🟡 first cut | `SandboxProfile` vs post-spawn attestation (`required ⊆ actual`). `UntrustedGenerated` fail-closed on native. Residual OS isolation is not `MOD-18`. Details: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md). **Not closed.** |
| M14 Resource Policy | ✅ | Schema/context quotas, standing grants, output broker, authority leases. Further typed policy is not a reopen of this gate. |
| M15 Real Evaluation | 🧪 | Reproducible per-cell artifacts. **Context** live `context-mech.v2` 12-cell A/C evidence is in `crates/agent-eval/evidence/context-mech/`. `add_test` is Tool Surface, not Context. Frozen `context-bench.v1` SPEC stays frozen. 300×3 parked. **Not closed.** |
| V2 Self-Iteration | 🔒 blocked | Until M12/M13/M15 close. The agent may grow capabilities, never evaluation or permission Core authority. |

Dependency: M10 → M11 → M12 → M13 → M14 → M15 → V2.
Engineering mainline is M12, then M13. Context live evidence runs in
parallel and does not retune GC. Tool Surface edit reliability may improve
in parallel, but it does not reorder or close either gate.

## Ordered route

1. Close M12 (brokerable effects; do not make raw shell transactional).
2. Close M13 (attestation of actual enforced capabilities; WASI is V2).
3. Keep M14 closed; do not reopen it as a sandbox dump.
4. V1 candidate: `context-mech.v2` 12-cell Context evidence exists; do not
   retune GC from it. Separately, canonical `edit.patch` has a current-source
   r4 dirty-tree diagnostic pass on the versioned v2 pack/v3 gate: non-conflict
   first patch 9/9, proactive stale route 3/3, raw truth and flow gate 12/12,
   with no observed patch failure, fallback, confirm read, recovery or unknown
   settlement. This supports the current path but is not formal acceptance or
   a general failure-rate estimate. Next run the unchanged frozen pack on a
   clean source tree. Core-managed prepare crash seams after authority intent,
   stage sync, and review record now have bounded fail-closed unit coverage;
   next extend external-race, abrupt-process, disk/journal fault, partial-batch
   fixtures and staged-byte accounting before broader filesystem reliability
   claims. Do not turn expected stale refusal into a first-attempt
   defect: grade the bounded read→stale→reread→retry state machine. Unit fixes
   or this diagnostic alone do not close M12, M13, or M15.
5. Formal M15 only from versioned per-cell artifacts. Do not use one
   A/B/C for every layer.
6. Execution Convergence V1 candidate gate (revised 2026-08-23 second
   review): before any V1 candidate claim, all of the following hold —
   (a) the Convergence Bench is green: three deterministic
   scripted-model scenarios (`retry_domain`, `operational_evidence`,
   `protocol_body`); (b) no unresolved typed obligation exceeds its
   bounded attempts under unchanged preconditions — read from the
   Obligation Ledger's UNRESOLVED BLOCKER warnings and the
   `ExecutionFrontier` event stream, not from the global scalar; and
   (c) hidden verification is green on the live A/C longflow cells.
   The global `frontier_no_advance_peak` metric is demoted to a
   diagnostic only: C r2 proved it can stay under threshold while a
   13-attempt process-guessing loop runs, because interleaved unrelated
   advances reset the counter. Evidence identity uses the Runtime
   `ArgumentDigest`; cache hit-rate claims must be backed by
   `ProtocolBodyCacheStats` events. This gate does not close
   M12/M13/M15 and does not reorder them.
7. V2 Self-Iteration last.
