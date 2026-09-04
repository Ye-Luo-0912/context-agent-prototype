# Code review — 2026-09-05

Full-repository consultation against the documented contract
([`AGENTS.md`](../../AGENTS.md) invariants, [`ROADMAP.md`](../ROADMAP.md)
phase exits, [`STATUS.md`](../STATUS.md) module table). Basis: working tree
at `d004836` (runtime code identical to the M15-closing candidate
`050aa8e`). No code was modified in this pass; every finding below was
re-verified against source with file:line evidence.

Context: M15 closed 2026-09-04 during this review (tenth v4 window PASS
12/12). The findings below are therefore framed purely against
**Phase 2 — Reliable Local Agent alpha** and Phase 3 packaging exits.

## Verdict summary

| # | Claim under review | Verdict |
| - | --- | --- |
| 1 | Manual and automatic checkpoints use two incompatible formats in one directory | **Confirmed** |
| 2 | The "kill/restart" product test is actually graceful shutdown/restart | **Confirmed** |
| 3 | Provider configuration still has silent fallbacks | **Confirmed** |
| 4 | `/status` cannot answer "how is the agent doing" | **Confirmed** |
| 5 | Automatic proof refresh waits inside the actor loop | **Confirmed** |
| 6 | TUI message list is unbounded | **Confirmed** |
| 7 | Schema regex recompiled per validation | Confirmed, **crate attribution corrected** (agent-contracts, not tool-runtime) |
| 8 | Oversized runtime/contract files | Confirmed (measured; several larger files exist) |
| 9 | Transaction-closure improvements landed as documented | **All 8 confirmed** |
| 10 | `rust-version` vs CI toolchain mismatch | Refined: **no `rust-version` is declared at all**; CI pins 1.97.1 |

## Product blockers (Phase 2 exit gaps), with evidence

### P2-BLK-1 — Checkpoint format split and retention hazard

Two writers share `.focus-agent/checkpoints/`:

- TUI `/checkpoint` writes raw pretty JSON with no envelope, checksum, sync
  or atomic rename: `crates/agent-tui/src/main.rs:500-535`
  (`serde_json::to_vec_pretty` L505, `tokio::fs::write` L519), file name
  `{run_id}-{now_ms}.json` (L502).
- The Runtime `CheckpointStore` writes a `runtime-checkpoint-envelope-v1`
  header (format + SHA-256 + payload bytes), temp file, fsync, atomic
  rename and count/byte retention: `crates/agent-runtime/src/checkpoint.rs:137-197`
  (caps at L100/L104: 32 files / 64 MiB), file name
  `checkpoint-{now_ms}-{runid}.json` (L162). Wired at
  `crates/agent-runtime/src/actor/safepoint.rs:146-149`.
- `/restore <path>` and startup `--restore` deserialize whole-file
  `RuntimeCheckpoint` and cannot read the envelope format:
  `crates/agent-tui/src/main.rs:537-562`, `606-618`.
- `/checkpoints` is a raw directory scan (`list_checkpoint_rows`,
  `main.rs:626-646`, cap 20) because `CheckpointStore` exposes no list API.
- **Hazard (sharper than previously recorded):** retention
  `prune_retained` (`checkpoint.rs:203-236`) counts **any `*.json`** in the
  directory (L212) with no name-pattern or header check. TUI raw exports are
  therefore counted and can be deleted by the store, and a fresh TUI file
  occupies a newest slot that can push a store envelope out of retention.

Fix direction: route every product entry through
`CheckpointStore` (add `list`), separate directories for authoritative
checkpoints vs user exports, and a one-time legacy decoder that fails
closed on unknown formats.

### P2-BLK-2 — Restart durability is only proven for orderly shutdown

`crates/agent-compose/tests/product_flow.rs` (landed in `bf52490`):

- `composed.shutdown().await.unwrap()` at L154 and L184 — graceful ordered
  teardown; no kill/abort anywhere (the module docstring's "kill" wording
  at L1 overstates what is tested).
- The persistent effect reservation journal is disabled
  (`effect_reservation_journal: None`, L95) and approvals are permissive
  (`PolicyApprovalGate::permissive()`, L82).

What is therefore **not** yet proven: SIGKILL/TerminateProcess of the real
process tree, checkpoint torn between temp-write and rename, event written
before its commit barrier, effect applied before ACK, sidecar vanishing,
context blob/catalog divergence, and no-duplicate-mutation after an actual
crash. ROADMAP Phase 2's exit ("kill/restart resumes a compatible safe
point without duplicate effects") still needs a genuine child-process
kill matrix (at minimum: after effect prepare; after apply/before ACK;
after checkpoint temp-write/before rename; after TurnCompleted/before
commit barrier).

### P2-BLK-3 — Provider configuration silent fallbacks

`crates/agent-compose/src/lib.rs` (`model_from_key` and helpers):

- `OPENAI_API_PROTOCOL`: `.ok().and_then(|v| OpenAiProtocol::parse(&v).ok()).unwrap_or_default()`
  (L187-190) — an invalid value silently becomes `Auto`, even though
  `OpenAiProtocol::parse` produces a typed error
  (`crates/provider-openai/src/lib.rs:57-66`) that the composition root
  discards.
- `OPENAI_CONTEXT_WINDOW`: `parse().unwrap_or(DEFAULT_DECLARED_CONTEXT_WINDOW)`
  (L218) — silent fallback to 128,000 (`provider-openai/src/lib.rs:111`).
- `max_output_tokens: 4096` hardcoded at L197.
- Seven independent booleans on `ComposeConfig` (L240, 252, 256, 261, 264,
  268, 283) plus Option knobs — the "bool soup" that a checked
  `ProviderProfile`/`ProductProfile` (returning
  `Result<_, ConfigurationErrors>` and digesting itself into events and
  checkpoints) would replace.

### P2-BLK-4 — `/status` shows four field groups, not agent state

`crates/agent-tui/src/state.rs:219-239` renders exactly: run id + status +
token totals; current task id + anchor revision; unresolved effect-ack
debt count; last checkpoint path. It does not show task goal/directive/
phase, next action, open loops, completion blockers, latest verification,
required-context misses, in-flight operation, recovery fence or provider
identity. Fix direction: a Runtime-owned read-only
`RuntimeStatusSnapshot` (consistent read model, not a second authority)
that TUI/CLI both render; also usable to resync a TUI after a Lagged
broadcast.

### P2-BLK-5 — Proof refresh blocks the actor

`crates/agent-runtime/src/actor/turn.rs:548`
(`verifier.verify_exact(request).await` inside
`refresh_proof_before_completion`, L495-611, `&mut self` held across the
await) is reached from tool completion (`tools.rs:1346` → `turn.rs:627`).
Because the actor loop (`actor/mod.rs:1563-1592`) awaits commands inline,
a slow verification script stalls status/cancel/shutdown/new-input/
checkpoint handling. Model and tool work are already tracked cancellable
operations (`InFlightOp { kind: OpKind::Model|Tool, cancel }`,
`mod.rs:209-234`); proof refresh should reuse that machinery
(`OpKind::ProofRefresh` + generation/basis validation on completion).

### P2-BLK-6 — Unbounded TUI transcript

`crates/agent-tui/src/state.rs:157` — `pub messages: Vec<UiMessage>` with
no cap or drain (push sites L242, 295, 576/586, 604), while
`context_transitions` is bounded at 100 (`MAX_PANEL_TRANSITIONS`, L30,
drained at L274-280/L491-495). Long sessions grow TUI memory without
bound; the fix is a bounded ring with disk-backed history via a paged
projection (the UI transcript must not re-become the append-only context
the runtime forbids).

## Corrections to the review input

- **Regex recompile** is real but lives in
  `crates/agent-contracts/src/schema_profile.rs` — pattern stored as
  `Option<String>` at compile (L257-266) and recompiled with
  `Regex::new(pattern).expect(...)` on every validation (L484-488), invoked
  per call from `agent-core/src/kernel/mod.rs:1053-1054`. The per-surface
  compile is in `agent-runtime/src/surface.rs:456-487`. `tool-runtime` is
  not involved (its own `Regex::new` uses in `tools/code.rs:76,585` and
  `tools/search.rs:143` are per-tool-call, not schema validation).
- **Toolchain**: the workspace declares **no `rust-version` at all**
  (`Cargo.toml` `[workspace.package]` has only edition/license/version);
  CI pins `dtolnay/rust-toolchain@stable` with `toolchain: 1.97.1`
  (`.github/workflows/ci.yml` L26-29, L82-85). The doc-consistency gate
  should therefore first *introduce* an MSRV, then check it against CI.

## Confirmed transaction-closure improvements (spot checks)

| Improvement | Evidence |
| --- | --- |
| Typed effect-ack debt persisted + reconciled on restore | `agent-contracts/src/operation.rs:474`; `agent-runtime/src/checkpoint.rs:401,494-507`; `actor/restore.rs:247-251`; events `event.rs:649-658` |
| Context admit plan → I/O → commit (no lock across disk IO) | `context-simple/src/engine.rs:1197-1271` |
| Store read taxonomy Missing / Corrupt / IoFailed | `context-simple/src/store.rs:91-107` |
| Required-context miss in MaterializedContext + events + completion readiness | `agent-contracts/src/context.rs:1945-2006`; `event.rs:192-202`; `agent-runtime/src/task.rs:654,1123-1125`; `actor/mod.rs:1457-1471,1520-1527` |
| `ToolFinished` typed execution facts | `event.rs:294-302`; `execution_facts.rs:43,56`; consumed in `agent-replay/src/lib.rs:334` |
| Committed-prefix replay | `agent-replay/src/recovery.rs:72-76,106-107,147-161,209-211` |
| Settlement projection toggle + same-state structured diff | `agent-compose/src/lib.rs:252-261`; `agent-runtime/src/prompt.rs:38-127`; consumed `actor/model.rs:708-843` |
| Schemas compiled on the immutable round surface | `agent-runtime/src/surface.rs:456-487`; `agent-core/src/kernel/mod.rs:1045-1067` |

## Size facts (bytes, measured 2026-09-05)

`agent-contracts/src/tool.rs` 175,789 · `agent-runtime/src/task.rs` 169,672
· `agent-workspace/src/lib.rs` 167,894 · `agent-eval/src/long_live.rs`
162,017 · `agent-eval/src/metrics.rs` 153,390 ·
`agent-runtime/src/actor/turn.rs` 118,603 · `actor/tools.rs` 106,971 ·
`actor/model.rs` 87,057 · `agent-contracts/src/context.rs` 137,896.
Any split should follow transaction boundaries, not line counts.

## Recommended order (aligned to ROADMAP phases)

1. `DOC-STATE-01` — done 2026-09-05: `docs/state.json`, `docs/CURRENT.md`,
   drift fixes, `archive/CONTEXT_RUNTIME_TODO.md` (this tranche).
2. `CHECKPOINT-PRODUCT-01` — P2-BLK-1.
3. `CRASH-RESUME-01` — P2-BLK-2, the four kill points.
4. `PRODUCT-PROFILE-01` — P2-BLK-3, digests into events/checkpoints.
5. `STATUS-SNAPSHOT-01` — P2-BLK-4.
6. `PROOF-OPERATION-01` — P2-BLK-5.
7. `TUI-ALPHA-01` — P2-BLK-6 + approval detail + Lagged resync.
8. `PACKAGE-01` — Windows/Linux binaries, checksums, clean-machine doctor
   (Phase 3, together with `LT-EVAL-06`).
9. Structured Context Frame (`ContextFrameCompiler`) may start as
   shadow/audit only after the Phase 2 shell is coherent; it must not
   precede items 2–7 and must not modify formal-prompt semantics outside
   its own gate.

Out of scope for now (per ROADMAP "Later, only from evidence"): run/task
database projection beyond the bounded read model, serial TaskGraph,
workers/multi-Agent, Self-Iteration (separately gated).
