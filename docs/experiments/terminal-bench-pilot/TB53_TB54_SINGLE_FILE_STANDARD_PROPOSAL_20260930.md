# TB53/TB54 single-file standard-mode Dynamic comparison (proposal)

**Plan snapshot:** local preparation originally made zero supplier calls. TB53 was later run and stopped with one usage-unknown request and a failed Runtime task; TB54 was not started under the approved stop rule. See [TB53 stop receipt](TB53_ABORTED_20260930.md) for observed results. TB51 had stopped earlier after the model combined two individually granted paths in one `edit.patch`: a file under `api/` and root-level `migrate.py`. Core correctly denied that composite effect, and the in-flight request usage from TB51 remains unknown; TB52 remained unstarted.

The existing system-prompt instruction to split cross-scope edits was not enough to prevent the model from combining them. For this new proposal, the host relay modifies only the task's outgoing Responses tool schema: it sets `edit.patch.files.maxItems=1` and `strict=true`; the Harbor adapter repeats the one-file rule. The OpenAI [Responses migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses) documents strict function schemas, and the [supported schema list](https://developers.openai.com/api/docs/guides/structured-outputs) includes array `maxItems`. The synthetic relay regression confirms that the request sent upstream carries this cap. EasyCLI's OAuth route has not yet been tested with the strict one-file schema. Core still checks and authorizes every write independently; the guard does not read or duplicate the Core grant rules.

TB51's 35 completed EasyCLI responses all reported `access_programs.cyber=standard`, which confirms the selected program on that route for completed calls. The in-flight 36th request remains unknown and is excluded from the new trial; it will not be retried.

## New trial identities

| Order | Identity | ELF | SHA-256 | Static preflight |
| --- | --- | --- | --- | --- |
| 1 | `TB53_SINGLE_FILE_STANDARD_BASELINE_DYNAMIC_56_20260930` | Old baseline | `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d` | `STATIC_READY_NO_PROVIDER_CALLS` |
| 2 | `TB54_SINGLE_FILE_STANDARD_TREATMENT_DYNAMIC_56_20260930` | Updated treatment | `119ba505c31f2444d6674980b62af0d052041ab57e4009ceceff97db568bb262` | `STATIC_READY_NO_PROVIDER_CALLS` |

Both slots lock EasyCLI GPT-6 Luna Responses/max, explicit standard safeguards, the single-file `edit.patch` tool schema, the same locked task package, and Debian 12 image. Their only ELF difference remains the Context headroom budget fix; the Python relay and task agent adapter are common to both.

## Limits and stop conditions

Each slot is capped at 56 decisions, 70 upstream attempts, 8,000,000 reserved input tokens, 440,000 output tokens, 1,400 tool attempts, 8,400 seconds, and the existing `$4` direct-API-equivalent estimate. The two-slot totals are at most 112 decisions, 140 attempts, 16,000,000 input tokens, 880,000 output tokens, and 2,800 tool attempts. Plus membership quota remains the account-side ceiling; the relay cannot enforce or verify its remaining balance. The direct-price estimate is not a Plus charge limit.

TB54 starts only if TB53 has a completed Runtime trial, an official Harbor grade, no Core denial, all provider usage settled, and every completed response reports `access_programs.cyber=standard`. Stop on any Core denial, unknown usage, a response with a non-standard access-program signal, an incompatible strict-schema response, or a noncomparable first slot. Keep all unknown reservations frozen; do not replay TB51's unknown request.

The 10-rule candidate grant preserves the prior scopes and counts and is proposed to expire at **2026-09-30 08:00 Asia/Shanghai**. It is a proposal file, not installed or used. SHA-256: `38fcbb9d26e602b87d23c4c05bed7dce4a74ce23fc8eb00c54f92aba0dee9769`; [candidate grant](../../../scripts/terminal_bench_pilot/grants_tb53_tb54_single_file_candidate.json).

Both static identities and their ABI/task/relay hashes are in the [preflight report](TB53_TB54_SINGLE_FILE_STANDARD_PREFLIGHT_20260930.json). This file records preparation only; running either slot still needs a new explicit authorization.
