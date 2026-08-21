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

### CORE-11 — HostToolPolicy registry & plugin admission (open, M12 next)

`agent-contracts/src/host_policy.rs` statically enumerates builtin
names (`fs.read`, `edit.patch`, `process.run`, ...). The fail-closed
direction is right — ToolSpec cannot self-authorize, and unknown names
get an empty `WorkspaceWrite` (no grant) — but the layering is wrong
long-term: contracts should define the vocabulary (`EffectIntent`,
`HostEffectBinding`, `HostToolPolicy` types); trusted composition
should own a `BuiltinHostPolicyRegistry`; `tool-runtime` provides the
implementations. Plugin admission (required before Self-Iteration)
must install operator-reviewed `HostToolPolicy` bindings — a manifest
schema is not an authority mapping. Until the registry exists,
external write plugins stay safely non-functional.

### CORE-12 — M13 attestation depth (open)

`SandboxCapabilities` booleans are the v1 floor. M13 acceptance should
upgrade to `SandboxAttestation { capabilities, backend,
backend_version, evidence }` so each enforced capability is
explainable (`fs_write_confined` → landlock ABI, `memory_quota` →
rlimit_as bytes). A boolean must not claim a stronger OS guarantee
than it delivers — `process_count_quota` was renamed from
`process_spawn_controlled` for exactly that (serde alias keeps the
wire compatible).

## Freeze (not a defect)

### CTX-11 — Execution Coherence V1

**Status: RC** (MOD-OBS-01 / MOD-PROG-01 / turn checkpointing landed
2026-08-21; freeze waits for the next live evidence pass). Do not
reimplement `ResumePoint`.
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
| MOD-AUTH-01 | `edit.patch files[]` multi-file authority widening → `EffectIntent::WorkspaceWriteSet` + all-paths `grant_matches` (2026-08-21; see PLATFORM_SECURITY.md) |
| MOD-AUTH-02 | Prepared effects report canonical `ActualWorkspaceWrite` (real path + real staged bytes); Core commit rejects `ActualExceedsApproved` outside the approved set (2026-08-21) |
| Sandbox floor | `UntrustedGenerated.required` now includes `fs_read_confined` + `cpu_quota` (still fail-closed on native until provable); `process_spawn_controlled` → `process_count_quota` with a wire-compat serde alias (2026-08-21) |
| Foreground ack | `ContextConsumptionAck.foreground_item_ids` + engine counter: foreground bodies the model saw are observable (weak signal; no residency / admission change) (2026-08-21) |
| TOOL-02 | `search.grep` `path` accepts a file target (file-or-directory), removing a class of `path_not_found` tool failures (2026-08-21) |
| EVAL identity | Live evidence runs refuse a dirty workspace by default (`--allow-dirty` opt-in); the manifest records `source_tree_digest` over HEAD tree + tracked diff + untracked `crates/` sources (2026-08-21) |
| EVAL-04 | Source-identity self-pollution: a live run's own untracked evidence output made every cell after the first report `git_dirty=true` (the `context-mech-convergence` manifests record this). Identity scans now exclude `crates/agent-eval/evidence` — run outputs are not tested sources (2026-08-21) |
| CTX-12 | Not a code divergence: the parity tests had spawned a 9-day-stale `target/debug/agent-context-service.exe` (cargo test never refreshes that artifact; `serde(default)` hid the wire drift). Fixed with a test freshness guard that fails closed with a rebuild hint (2026-08-21). Scoped test runs need `cargo build -p agent-context-service` first. |
| PROV-01 | `provider-openai` loopback wire test failed through machine-wide proxies (Clash/V2Ray WinINET interception → gateway 502). Fixed with `OpenAiProvider::with_client` + a `no_proxy` test client (2026-08-21); production `new` keeps auto system proxy. |

Do not start sourced `EpisodeOutcome`, GC retune, or a second ResumePoint
from this index.
