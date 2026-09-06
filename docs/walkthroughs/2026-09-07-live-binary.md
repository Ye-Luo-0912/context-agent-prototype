# Real-binary live walkthrough (M16-08 live portion)

- Date: 2026-09-07
- Entry: the real `agent-tui` binary (headless `--prompt` / `--work`,
  JSONL out), provider profile `gpt-5.6-luna @ api.pinaic.com/v1`
  (responses), standing grants via `--grant-file`, bounded
  `--max-rounds`. Disposable workspaces; this record is a product
  walkthrough, not a model-success score.

## 1. fix a small bug (user's own file present)

Workspace held `calc.py` (`add` returned `a - b`) plus the user's own
`config.txt`. Grant covered only `calc.py`.

- Exit 0; `add` returns `a + b`, `mul` unchanged, `config.txt`
  byte-identical.
- `session_end`: `task_state: awaiting_operator_review` — the closure
  semantics flow through the real process.

## 2. add a small feature (two files, --work)

`config.DEFAULTS` gained `timeout_ms=5000`; `app.py` gained
`TIMEOUT_MS = get("timeout_ms")`. host/port untouched. Both writes
landed via `edit.replace` under per-file grants.

- Exit 3 (`approval_denied`): one late `edit.patch` call batching both
  files was refused. Finding: a multi-file write intent requires a
  single grant whose prefix covers the whole set
  (`agent-core/src/approval.rs`, by-design fail-closed), so per-file
  grants can never admit a batched `edit.patch`. Recorded as a usability
  limitation in `AUDIT_TODO.md`, not relaxed here.

## 3. interrupt → cold restore → continue (cross-process)

`--work --max-rounds=1` on a read-first task: exit 2, `round_budget`,
no writes yet (the model spent the round reading).

Two product defects were found here and fixed in the same stage:

- **A read-only final round left nothing to restore.** The budget-stop
  safe point only fired on checkpoint debt, so a read-only round wrote
  no checkpoint and `--restore=latest` failed (os error 3). Fix: the
  budget stop now accrues a `BudgetStopYield` debt whenever a task is
  active, forcing a resumable snapshot.
- **Shutdown killed in-flight safe-point writes.** The store held a
  `.checkpoint-*.json.tmp` remnant: the headless process exited while
  the write was running. Fix: actor shutdown now awaits the in-flight
  write before the kernel stop, keeping the `CheckpointDurable` audit
  event inside the same journal flush.

Re-run after the fixes: session 1 exits 2 with a durable checkpoint;
session 2 (`--restore=latest --continue`) exits 0 with `summary.txt`
(intro/method/results) and `index.txt` exactly as specified. Regression:
`m16_restore.rs::product_budget_stop_after_a_read_only_round_still_lands_a_resumable_checkpoint`.

## Minor finding (recorded, not fixed)

- In the restored session, `session_end.task_state` reports `none`
  because the headless drain only tracks task activity from live events;
  the restored active task is invisible to it. Under-reports; no
  over-claiming. Backlog in `AUDIT_TODO.md`.

## Without a provider (also covered)

`crates/agent-tui/tests/real_binary_startup.rs` drives the built
executable: a missing model config fails with the fix named in stderr
and leaves no `.focus-agent` state (M16-01 preflight order); an
`AGENT_DEMO=1` headless run completes with `task_state:
awaiting_operator_review` in the JSONL.
