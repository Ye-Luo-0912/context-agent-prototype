# TB55/TB56 single-file standard-mode Dynamic repeat

**State: both user-authorized slots completed and are comparable.** TB53 failed after one unsettled request at the 600-second boundary, so TB54 was correctly stopped. The relay now records a fixed Responses terminal-event kind in future receipts; this adds diagnostic evidence without changing request retry behavior or Core grants. Paired outcome and limits are recorded in [TB55/TB56 comparison](TB55_TB56_COMPARISON_20260930.md), with separate [TB55](TB55_RECEIPT_20260930.md) and [TB56](TB56_RECEIPT_20260930.md) receipts.

## Identities and controls

| Order | Identity | ELF | SHA-256 | Static result |
| --- | --- | --- | --- | --- |
| 1 | `TB55_SINGLE_FILE_STANDARD_BASELINE_DYNAMIC_56_20260930` | Old baseline | `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d` | `STATIC_READY_NO_PROVIDER_CALLS`; ABI PASS |
| 2 | `TB56_SINGLE_FILE_STANDARD_TREATMENT_DYNAMIC_56_20260930` | Updated treatment | `119ba505c31f2444d6674980b62af0d052041ab57e4009ceceff97db568bb262` | `STATIC_READY_NO_PROVIDER_CALLS`; ABI PASS |

Both arms use EasyCLI GPT-6 Luna Responses/max, explicit standard-mode safeguards, strict one-file `edit.patch`, the same locked task and Debian 12 image, and the relay telemetry fix. Their only ELF difference is the existing Context headroom change. TB56 starts only if TB55 completes with a comparable official grade, fully settled usage, no Core denial, and standard-mode metadata on all completed responses. Any unknown usage, nonstandard mode, Core denial, incompatible schema, or noncomparable first slot stops the sequence. TB53's unknown request remains frozen and will not be replayed.

Each slot retains the prior authorized envelope: at most 56 decisions, 70 provider attempts, 8,000,000 reserved input tokens, 440,000 output tokens, 1,400 tool attempts, 8,400 seconds, and a $4 direct-API-equivalent peak-miss estimate. The current model list-price snapshot estimates a $1.02 peak-miss envelope; neither estimate is a Plus subscription charge cap. The Plus account enforces its own quota; the relay cannot verify or enforce remaining Plus quota.

The desktop helper failed twice with `failed to write kernel assets: 系统找不到指定的路径`, so the live EasyCLI quota percentage could not be refreshed before this window. Static preflight made no supplier calls. The user's continuation approval applies to the prior bounded single-file trial envelope; this limitation is retained in the evidence record.

The new 10-rule grant candidate preserves the same write scopes and process counts as TB53/TB54; only expiry moves to **2026-09-30 16:00 Asia/Shanghai** so both maximum-duration slots can finish. SHA-256: `164ba5e519aaf09b857c34c8365675db0dda7c579819d8ec96b27a0ab777f152`; [candidate grant](../../../scripts/terminal_bench_pilot/grants_tb55_tb56_single_file_candidate.json). It is mounted read-only for a run and does not install or persist policy on the host.

Both identities, binary/task/image hashes, relay hash, and zero-call static evidence are in the [runtime preflight](TB55_TB56_RUNTIME_PREFLIGHT_20260930.json). The relay accepts only the bounded one-file tool schema and now records `response.completed`, `response.failed`, or `response.incomplete` in receipts without copying provider error details; the official event names are listed in the [Responses streaming guide](https://developers.openai.com/api/docs/guides/streaming-responses) and [deployment checklist](https://developers.openai.com/api/docs/guides/deployment-checklist).
