# Recovery runbook

Operator procedures for the failure modes a local run can hit. Every
procedure is safe by construction: the runtime validates before it
mutates, and nothing here can make a half-committed state worse.

## State on disk (everything lives under `<workspace>/.focus-agent/`)

| Path | What it is |
| --- | --- |
| `checkpoints/` | Envelope artifacts (`runtime-checkpoint-envelope-v1`, SHA-256 verified on load) |
| `traces/` | The event journal (`RuntimeEventEnvelope` JSONL, one file per run) |
| `authority/` | Effect reservation journal, workspace change journal (`changes.jsonl`) |
| `artifacts/` | Spilled tool output (large bodies never live in the trace) |
| `diagnostics/` | `/diag-export` bundles |

## Cold resume after a crash / kill

1. Restart with the same workspace:
   `agent-tui --restore=<checkpoint-path> <workspace>`
2. The checkpoint is read, envelope-verified (or legacy-raw decoded) and
   validated **before** the runtime or workspace is touched. Invalid
   input exits with a visible error and zero mutations.
3. Prefer the newest artifact from `/checkpoints` (the store listing).
   Restore replays the committed prefix; unresolved effect-ack debts are
   re-fenced and reconciled against the reservation journal's durable
   truth — an Applied settlement clears, an Ambiguous one keeps the
   recovery fence until you reconcile.
4. Invariants proven by the crash matrix (`agent-compose
   tests/crash_resume.rs`): the committed write appears exactly once,
   the uncommitted turn never resurrects, torn checkpoint temp files are
   never listed as checkpoints, and a prepared-but-undispatched effect
   reconciles NotApplied on any number of restarts.

## A `task.complete` will not go through

The completion gate refuses with the blocker list (verification currency,
acceptance coverage, open loops, failed commands). The model enters a
bounded semantic repair episode: each refusal must strictly lower the
typed blocker potential or the no-progress counter advances; repeated
cycling ends in an audited text-only handoff that ends the turn without
completing the task. Operator actions: inspect `/status` (blockers,
debts, latest checkpoint), fix the named gap or answer with new input
(new input resets the repair episode).

## A slow verify is freezing the round

Inline completion-time proof refresh holds the actor while the recipe
runs (bounded by the verifier's own timeout). The opt-in
`defer_proof_refresh` compose flag moves the host verifier outside the
actor loop; a deferred resume that lands after the turn finished drops
the parked intent with a visible warning, the refreshed proof stays
recorded, and the next `task.complete` is admitted inline. A verifier
panic becomes a typed error — a held turn cannot wedge.

## The UI fell behind (`Lagged` warning)

A broadcast receiver that dropped events says so by name. The status
projection is resynced automatically from the durable journal
(`traces/*.jsonl`, newest 16 files, 32 MiB each); the transcript rows
that were dropped are gone from the screen but not from the journal —
`agent-replay <trace>` reads the full history offline.

## Suspicious checkpoint files in the store

`--doctor <workspace>` lists the verified artifacts and names any
checkpoint-like `.json` files it had to skip (manual exports, torn
writes) as warnings. Foreign files are never deleted by the store and
never counted against retention — delete them by hand only after
reading them; a torn temp file (`*.tmp`) is always safe to remove.

## Diagnostics for a bug report

Run `/diag-export` in the session (or collect `diagnostics/diag-*.txt`
afterwards): version, status-projection snapshot, bounded transcript
tail and the checkpoint index. Key-free by construction. For deeper
analysis attach the run's `traces/<run>.jsonl` — it replays
deterministically via `agent-replay`.
