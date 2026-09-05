# Configuration reference

Everything is environment-variable driven and checked at startup: an
invalid value is a visible error naming the variable, never a silent
default. The checked, key-free identity (provider profile digest) is
printed at startup and persisted into every checkpoint. The quickstart
lives in [`INSTALL.md`](INSTALL.md); this is the full reference.

## Model selection

| Variable | Values | Default | Notes |
| --- | --- | --- | --- |
| `AGENT_DEMO` | `1` / `true` | off | Explicit demo transport; wins over any key. A run without a key and without this flag is a startup error — never a silent mock. |
| `OPENAI_API_KEY` | non-empty string | — | Required unless demo. Never logged, never in digests. |
| `OPENAI_BASE_URL` | URL | `https://api.openai.com/v1` | Blank/empty counts as invalid when set. |
| `OPENAI_MODEL` | model id | `gpt-4o-mini` | Blank/empty counts as invalid when set. |

## Provider behavior (strict parsing: garbage is a startup error)

| Variable | Default | Range |
| --- | --- | --- |
| `OPENAI_API_PROTOCOL` | `auto` | `auto` / `responses` / `chat` |
| `OPENAI_CONTEXT_WINDOW` | `128000` | integer ≥ 1024 |
| `OPENAI_MAX_OUTPUT_TOKENS` | `4096` | integer ≥ 1 |
| `OPENAI_TEMPERATURE` | unset = provider default | 0.0 – 2.0 |

Sampling is an **explicit operating point**: unset means the declared
provider-default (recorded as such in the profile digest); set means the
temperature field is pinned on every wire request under both protocols.

## Serving identity

Every run prints the provider profile banner and persists the digest
into every checkpoint's run metadata:

```
provider profile: <model> @ <base_url> protocol=responses context_window=128000 max_output_tokens=4096 sampling=provider-default digest=0123456789abcdef…
```

Two runs compare operating points by comparing `provider_profile_digest`.
It is SHA-256 over the canonical identity JSON and contains no key
material. See [`COMPATIBILITY.md`](COMPATIBILITY.md) for the schema.

## Runtime switches (composition root)

| Flag | Default | Effect |
| --- | --- | --- |
| `--read-only` | off | Every write/process call is denied by policy; cannot combine with `--grant` or `--restore`. |
| `--grant=<JSON>` | none | Standing grants for write/process tools; revoke with `/revoke <grant-id>`. |
| `--restore=<path>` | none | Cold resume; validates the checkpoint before any mutation. Accepts envelope artifacts and legacy raw JSON; bare artifact names resolve inside `checkpoints/`. |
| `--effect-reservation-journal=<path>` | `<state>/authority/broker-reservations.jsonl` | Persistent reservation barrier for crash reconciliation. |
| `--context=dynamic\|append\|rolling\|service` | `dynamic` | Context engine selection. `service` spawns the sidecar and stays experimental. |
| `defer_proof_refresh` / `shadow_context_frame` (compose flags) | off | Deferred host-verifier execution / shadow Context Frame manifest emission. Runtime flags, not CLI flags yet. |

## Context policy (engine)

`--context` selects the engine: dynamic working set (default, the C
baseline), append-only (A), rolling summary (B), or the process-boundary
adapter (`service`, experimental). Retention/GC parameters are frozen
measurement surfaces — see [`CONTEXT_LIFECYCLE.md`](CONTEXT_LIFECYCLE.md).

## Checkpoints

`/checkpoint` (or the automatic safe points) writes to
`<state>/checkpoints/` as envelope artifacts with count/byte retention
(32 files, 64 MiB). `/restore` verifies the envelope checksum (or decodes
legacy raw JSON) and validates the payload **before** any mutation. A
fresh workspace reports "no checkpoints yet"; the store directory is
created on first save. Payload version is
`RUNTIME_CHECKPOINT_VERSION = 4`; other versions are rejected — see
[`COMPATIBILITY.md`](COMPATIBILITY.md).
