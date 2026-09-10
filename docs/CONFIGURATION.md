# Configuration reference

Everything is environment-variable driven and checked at startup: an
invalid value is a visible error naming the variable, never a silent
default. The checked, key-free identity (provider profile digest) is
printed at startup and persisted into every checkpoint. The quickstart
lives in [`INSTALL.md`](../INSTALL.md); this is the full reference.

## Model selection

| Variable | Values | Default | Notes |
| --- | --- | --- | --- |
| `AGENT_DEMO` | `1` / `true` | off | Explicit demo transport; wins over any key. A run without a key and without this flag is a startup error — never a silent mock. |
| `OPENAI_API_KEY` | non-empty string | — | Required unless demo. Never logged, never in digests. |
| `OPENAI_BASE_URL` | URL | `https://api.openai.com/v1` | Blank/empty counts as invalid when set. |
| `OPENAI_MODEL` | model id | `gpt-4o-mini` | Blank/empty counts as invalid when set. |

## Provider behavior (strict parsing: garbage is a startup error)

Prompt layout is a provider-independent runtime composition choice. The
default is `agent_contracts::PromptLayout::CurrentStateLast`: the complete
current tool catalog and Focus/TaskAnchor/TaskProgress follow the retained
execution history, preserving their text and roles. Context selection,
scoring, GC, schema selection and budgets are unchanged.

For an endpoint that requires the historical placement, or a controlled
behavior comparison, a Rust composition root can call
`agent_compose::compose_with_prompt_layout(config, PromptLayout::Legacy)`;
direct compositions can use `RuntimeServices::with_prompt_layout`. This is
a typed construction option, **not a new environment variable or remote
client setting**. Older serialized ModelInput values default to Legacy.
Request metadata names the layout. After final packing, `ModelInput::into_request`
also binds one typed `PromptReuseBoundary` to the retained prefix and tool schemas.
This hint alone adds no vendor parameter and claims no cache hit.

For opt-in transport diagnostics, Rust callers can use
`OpenAiProvider::complete_stream_observed` with a per-call `OpenAiCallObserver`.
It fingerprints the actual HTTP body and reports optional model/cache counters;
it neither changes the payload nor adds a cache setting. Normal Runtime calls
do no diagnostic hashing or storage. See [KV diagnostics](reviews/2026-09-10-cache-live/DIAGNOSTICS.md)
and the [isolated live result](reviews/2026-09-10-cache-live/ISOLATED_REPORT.md).

`OpenAiCallObserver::on_http_error` additionally reports the protocol, HTTP
status, and an optional exact service-reported cache rejection. It exposes no
arbitrary error body and does not change failure/retry behavior. The current
configured gateway/model explicitly rejected the breakpoint; see the
[capability finding](reviews/2026-09-10-cache-live/CAPABILITY.md). This feature
remains opt-in and the default does not send caching extensions.

| Variable | Default | Range |
| --- | --- | --- |
| `OPENAI_API_PROTOCOL` | `auto` | `auto` / `responses` / `chat` |
| `OPENAI_PROMPT_CACHE_MODE` | `provider_default` | `provider_default` / `responses_explicit`; explicit mode requires `OPENAI_API_PROTOCOL=responses` |
| `OPENAI_RESPONSES_REASONING_EFFORT` | `provider_default` | `provider_default` / `none` / `low` / `high` / `max`; a pinned value requires `OPENAI_API_PROTOCOL=responses` |
| `OPENAI_CONTEXT_WINDOW` | `128000` | integer ≥ 1024 |
| `OPENAI_MAX_OUTPUT_TOKENS` | `4096` | integer ≥ 1 |
| `OPENAI_TEMPERATURE` | unset = provider default | 0.0 – 2.0 |

Sampling is an **explicit operating point**: unset means the declared
provider-default (recorded as such in the profile digest); set means the
temperature field is pinned on every wire request under both protocols.

Responses reasoning is also explicit: `provider_default` sends no `reasoning`
field and preserves historical profile digests; a pinned value sends
`reasoning: {"effort": "<value>"}` and changes the profile digest. Unsupported
values/protocol combinations fail at startup; model-specific support is still
the endpoint's responsibility. Rust callers can select the same setting with
`OpenAiProvider::with_responses_reasoning_effort`.

For DeepSeek Flash, the currently documented model is `deepseek-flash` at
`https://api.deepseek.com`, with a native Responses endpoint. The bounded cache
comparison pins reasoning to `none`, which DeepSeek documents as disabling
thinking, and uses `provider_default` caching. DeepSeek learns shared prefixes
automatically, so its comparison includes three distinct state changes per
layout. See [DeepSeek's Responses reference](https://api-docs.deepseek.com/api/create-response/)
and [cache rules](https://api-docs.deepseek.com/guides/kv_cache/). The test runner
`docs/reviews/2026-09-10-cache-live/run_deepseek.py` accepts a process-local
`DEEPSEEK_API_KEY` or stdin and never writes it into the workspace configuration.

`responses_explicit` is an operator declaration that the configured endpoint/model
supports Responses content-block caching; compatibility URLs and model aliases do
not enable it automatically. With a valid common boundary, the provider adds one
`prompt_cache_breakpoint: {"mode":"explicit"}` to the last retained evidence
message (or policy for Legacy), and `prompt_cache_options: {"mode":"explicit"}`.
Every message, its role, the complete current directive and tool definitions are
still sent. A missing/stale hint sends the ordinary full request with no cache
fields. Unsupported endpoints fail visibly; parameters are not silently removed
and retried. Rust callers can select the same mode with
`OpenAiProvider::with_prompt_cache_mode`. Explicit mode changes the provider profile
digest; default mode preserves the historical digest. TTL and cache routing keys
are left to the provider. See [boundary implementation and live check](reviews/2026-09-10-cache-live/BOUNDARY.md)
and the [official Responses caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).

## Serving identity

Every run prints the provider profile banner and persists the digest
into every checkpoint's run metadata:

```
provider profile: <model> @ <base_url> protocol=responses context_window=128000 max_output_tokens=4096 sampling=provider-default prompt_cache=provider_default responses_reasoning=provider_default digest=0123456789abcdef…
```

Two runs compare operating points by comparing `provider_profile_digest`.
It is SHA-256 over the canonical identity JSON and contains no key
material. See [`COMPATIBILITY.md`](COMPATIBILITY.md) for the schema.

## Runtime switches (composition root)

| Flag | Default | Effect |
| --- | --- | --- |
| `--read-only` | off | Every write/process call is denied by policy; cannot combine with `--grant` / `--grant-file` or `--restore`. |
| `--grant=<JSON>` | none | Standing grants for write/process tools; revoke with `/revoke <grant-id>`. |
| `--grant-file=<path>` | none | Same grants from a JSON object or array (repeatable; combined cap 16). Fail-closed if missing/invalid. Also valid for the TUI. |
| `--jsonl-out=<path>` | stdout | Headless: write JSONL to a file instead of stdout. Requires `--prompt` or `--continue`. |
| `--prompt=<text>` | unset | Headless: one user message (`-` reads stdin). JSONL events on stdout unless `--jsonl-out` is set. |
| `--work` | off | Headless long-task entry (same composition as TUI `/work`). Requires `--prompt`. |
| `--continue` | off | Headless: continue the restored/active task's stored directive. Cannot combine with `--prompt`. |
| `--max-rounds=<N>` | runtime default (16) | Finite model-round budget for one execution segment (TUI and headless). |
| `--restore=<path>` | none | Cold resume; validates the checkpoint before any mutation. Accepts envelope artifacts and legacy raw JSON; bare artifact names resolve inside `checkpoints/`. |
| `--effect-reservation-journal=<path>` | `<state>/authority/broker-reservations.jsonl` | Persistent reservation barrier for crash reconciliation. |
| `--context=dynamic\|append\|rolling\|service` | `dynamic` | Context engine selection. `service` spawns the sidecar and stays experimental. |
| `defer_proof_refresh` / `shadow_context_frame` (compose flags) | off | Deferred host-verifier execution / shadow Context Frame manifest emission. `--defer-proof` opts the product path in; shadow Frame is still compose-only. |

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
