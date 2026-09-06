# Real-binary live walkthrough (M16-08 live portion)

- Date: 2026-09-07
- Entry: the real `agent-tui` binary (headless `--prompt`, JSONL out)
- Provider: **NOT_RUN** — no `OPENAI_API_KEY` in the environment at this
  time (the 2026-09-06 [f6](2026-09-06-f6.md) record remains the live
  evidence for the in-process product path with a real provider).

## What ran without a provider (real binary, demo transport)

`crates/agent-tui/tests/real_binary_startup.rs` drives the built
executable directly:

- a missing model config fails with the fix named in stderr and leaves
  **no** `.focus-agent` state behind (M16-01 preflight order);
- an `AGENT_DEMO=1` headless run completes end to end, and the JSONL
  `session_end` row carries `task_state: awaiting_operator_review` —
  the closure semantics flow through the real process.

## What needs a real provider (NOT_RUN)

- live re-validation of the turn-end closure display, `/continue` after a
  budget stop, and the PROCESS-01 watchdog/ledger under a real model;
- the three daily tasks re-run through the real binary headless entry.

Per the standing convention these stay `NOT_RUN` until credentials are
available; they do not gate the already-landed code paths, which carry
their own deterministic E2E coverage (`tui_e2e_*`, `m16_restore.rs`,
`route_flow.rs`, `crash_resume.rs`).
