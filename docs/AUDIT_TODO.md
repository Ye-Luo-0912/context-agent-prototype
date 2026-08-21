# Audit follow-up

Confirmed **open** defects only. Closed write-ups stay in git history of
this file; do not copy them back and do not reopen them as new work.

- Invariants: `AGENTS.md`
- Now/freeze/P0: [`STATUS.md`](STATUS.md)
- Execution: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
- Sandbox/M12: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md)
- Gates: [`ROADMAP.md`](ROADMAP.md)

M12/M13 must close before Self-Iteration. Do not add a database, vector
search, or learned ranking. Do not claim a milestone complete because
happy-path tests pass.

## Open P0 — trusted execution

### CORE-01 — M12/M13 residual (not closed)

First cuts landed: trusted `HostToolPolicy`, structured `EffectIntent`
(`ExecArgv` / `ShellExec`), `HostLifecycle` restart circuit,
`SandboxProfile` vs post-spawn `SandboxCapabilities`. External process
capabilities stay Disabled by default. Generic `shell.exec` /
`process.run` / `process.session` stay non-transactional (Core identity
before spawn, kill-then-reap, no rollback of child mutations).

Remaining OS isolation is the residual, not a new `MOD-18` slice:

- Linux UDP / raw / pathname-Unix
- Linux absolute OS-level reads
- Windows OS-level network
- I/O bandwidth quotas
- seccomp / AppContainer

`UntrustedGenerated` fails closed on native. WASI is V2. Matrix:
[`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md).

Do not make raw shell transactional. Do not close M12/M13 from the
first cut.

### CORE-10 — protocol remaining (not a transport swap)

`PLAT-00`–`PLAT-04` containment/protocol proof is landed. Remaining:

- PLAT-06 multiplexing (stay single-inflight in v0)
- PLAT-07 adapter envelope migration
- PLAT-08 Named Pipe/UDS (later)

Named pipes/UDS are not a fix for CORE-01. V1 still trusts Runtime in
the same address space.

## Freeze (not a defect)

### CTX-11 — Execution Coherence V1

**Status: freeze candidate.** Do not reimplement `ResumePoint`.
Contract: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md).

## Open P2 — evaluation

### EVAL-01 — M15 live evidence is not yet auditable acceptance

Live cells must remain rebuildable from versioned per-cell artifacts
(manifest, events, verify, workspace hash). Partial `agent-eval.cell.v1`
bundles exist; executable hidden build/tests for a large suite do not.
Do not close M15 from smoke, `add_test`, or the 30-task pilot.

### EVAL-02 — layer the evals; do not reuse one A/B/C

Frozen `context-bench.v1` SPEC / pack digest stay frozen. Wave-1 live
(27 cells) is historical evidence under
`crates/agent-eval/evidence/context-bench-wave1/`. **Context** live
`context-mech.v2` (A/C × 3 tasks × 2 repeats = 12 cells) is under
`crates/agent-eval/evidence/context-mech/`. `add_test` is Tool Surface
(`historical_context=0`), not Context. Do not collect 300×3. Do not
retune GC from that live or from an `add_test` cell. Do not treat
`Likely optimization target` as a modification order.

## Closed archive (index only)

Full text: git history of this file.

| ID | Closed as |
| --- | --- |
| 2026-08-10 repair pass | Workspace prefix, git.diff, focus/restore fences, context-service parity, journal/restore |
| CTX-01..CTX-10 | Episode, residency, fetch/search persist, store, Storage GC, GC ops, materializer, mid-turn signals, clocks, TaskAnchor |
| CTX-06..CTX-09 | GC/storage ops, materializer budget, working-set signals, lifecycle clocks |
| CORE-02..CORE-09 | Turn durability, checkpoint, output broker, System-role leak, cancel/process cleanup, TOCTOU opens, standing grants, schema budget |
| TOOL-01 | `search.grep` cancellation |
| TOOL-ENV-01, TOOL-EDIT-01, TOOL-VIEW-01, TOOL-ERROR-01 | Tool-quality preflight 2026-08-17 |

Do not start sourced `EpisodeOutcome`, GC retune, or a second ResumePoint
from this index.
