# Installing and running the local agent

The product binary is `agent-tui`: one workspace, one checked provider
profile, bounded builtin tools, human-approved effects, verifiable
completion and cold resume.

## Prerequisites

- Rust 1.97.1 (the toolchain CI pins; `rustup` default is usually newer —
  any recent stable that compiles the 2024 edition works).
- No database, no daemon, no service accounts. Everything lives in the
  workspace directory you point the binary at.

## Install

From a checkout of this repository:

```bash
cargo install --path crates/agent-tui --locked
```

This installs `agent-tui` into `~/.cargo/bin`. Alternatively run it from
the checkout without installing:

```bash
cargo run -p agent-tui -- <workspace-dir>
```

## Configure a model

Configuration is checked at startup; an invalid value is a visible error,
never a silent default. Either pick the explicit demo transport:

```bash
export AGENT_DEMO=1
```

or configure a real OpenAI-compatible provider:

| Variable | Meaning | Default when unset |
| --- | --- | --- |
| `OPENAI_API_KEY` | provider key (required unless `AGENT_DEMO=1`) | — |
| `OPENAI_BASE_URL` | API root | `https://api.openai.com/v1` |
| `OPENAI_MODEL` | model id | `gpt-4o-mini` |
| `OPENAI_API_PROTOCOL` | `auto` / `responses` / `chat` | `auto` |
| `OPENAI_CONTEXT_WINDOW` | declared context window | `128000` |
| `OPENAI_MAX_OUTPUT_TOKENS` | output cap | `4096` |
| `OPENAI_TEMPERATURE` | pin the sampling temperature (0.0–2.0); unset means the provider default | provider default |

The checked, key-free serving identity (`provider profile: … digest …`) is
printed at startup and persisted into every checkpoint.

## First run

```bash
agent-tui --doctor .      # diagnose this machine/workspace
agent-tui .               # interactive TUI over the current directory
```

Inside the TUI: type a message and press Enter; `/help` lists the product
commands (`/checkpoint`, `/checkpoints`, `/restore`, `/grants`, `/revoke`,
`/cancel`, `/status`). Write and process tools ask before they act; a
missing key or an invalid checkpoint fails before any mutation.

## Headless (scripts)

The same binary, composition, tools and restore path, without the TUI.
Events are JSONL on stdout; banners go to stderr. A missing human never
implies allow-all: ungranted writes and process calls are denied.

```bash
# read-only question (demo transport)
AGENT_DEMO=1 agent-tui --prompt="demo: list files" .

# unattended write: a matching standing grant is required
AGENT_DEMO=1 agent-tui --prompt="demo: write hello" \
  --grant='{"id":"hello","risk":"WorkspaceWrite","target":{"workspace_path_prefix":"hello.txt"},"constraint":{},"expires_at_ms":4102444800000}' \
  .

# long-task entry + bounded segment, then later continue
agent-tui --work --prompt="migrate the config module" --max-rounds=24 --grant='...' .
agent-tui --restore=latest --continue --max-rounds=24 --grant='...' .
```

`--prompt=-` reads the prompt from stdin. `--yes` / `--allow-all` are
rejected. Exit codes: `0` completed without a denied mutation, `2` round
budget (use `--continue`), `3` approval denied, `1` error/timeout/cancel.
The last JSONL row is `{"schema":"agent.headless.v1","kind":"session_end",...}`.

## Editor / task invoke

No daemon. An editor task is the same headless binary: a prompt, a grant
file (so Windows/task JSON does not have to quote a grant on argv), and
optional JSONL capture.

`grants.json` (object or array; matching standing grants only — this is
not allow-all):

```json
{
  "id": "src-writes",
  "risk": "WorkspaceWrite",
  "target": { "workspace_path_prefix": "src" },
  "constraint": {},
  "expires_at_ms": 4102444800000
}
```

```bash
agent-tui --work --prompt="fix the parser" \
  --grant-file=grants.json \
  --jsonl-out=.focus-agent/last-headless.jsonl \
  --max-rounds=24 \
  .
```

A VS Code / Cursor task can call that command with `${workspaceFolder}` as
the workspace argument and as the process working directory so relative
`--grant-file` / `--jsonl-out` paths resolve there. Banners stay on stderr;
events go to `--jsonl-out` (or stdout if omitted). `--grant-file` is also
valid for the interactive TUI. The parent directory of `--jsonl-out` must
already exist (`.focus-agent/` is created when the workspace opens).

## Checkpoints and cold resume

- `/checkpoint` writes the same atomic, checksum-verified envelope the
  automatic safe points use (`.focus-agent/checkpoints/`).
- A killed session resumes from the shell:
  `agent-tui --restore=<checkpoint-path> <workspace>` — the checkpoint is
  read and validated before anything is touched. Legacy raw-JSON
  checkpoints are accepted; anything else fails closed.

## Upgrade notes

- Checkpoint payloads are versioned (`RUNTIME_CHECKPOINT_VERSION = 4`);
  older versions are rejected with an explicit error instead of being
  silently migrated.
- The checkpoint envelope (`runtime-checkpoint-envelope-v1`) is stable;
  pre-envelope manual exports remain readable through the legacy decoder.
- `sampling` and the provider profile digest are recorded in checkpoints
  from this version on.

## Artifacts on disk

Everything the runtime writes lives under the workspace's `.focus-agent/`:
`checkpoints/` (envelope store), `traces/` (event journal), `authority/`
(reservations, change journal), `artifacts/` (spilled tool output).

## Uninstall

Remove the binary (`cargo uninstall agent-tui`) and delete `.focus-agent/`
inside any workspace you no longer need.
