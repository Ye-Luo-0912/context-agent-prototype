# TB51/TB52 explicit standard-mode Dynamic comparison (proposal)

**Original proposal state: approved and statically ready. Execution update: TB51 was stopped by Core `edit.patch` denial after 35 settled provider calls and one usage-unknown in-flight call; TB52 was not started.** See [TB51 stop report](TB51_ABORTED_20260929.md). TB50 had established that a short EasyCLI request completes when `access_programs.cyber="standard"` is explicit, but it did not record the server's selected-program metadata or score a task. The plan below records the approved pre-run comparison and its bounds; it does not authorize resuming after TB51's noncomparable outcome.

## Trial identities

| Order | New identity | ELF | SHA-256 | Static preflight |
| --- | --- | --- | --- | --- |
| 1 | `TB51_STANDARD_BASELINE_DYNAMIC_56_20260929` | Old baseline | `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d` | `STATIC_READY_NO_PROVIDER_CALLS` |
| 2 | `TB52_STANDARD_TREATMENT_DYNAMIC_56_20260929` | Updated treatment | `119ba505c31f2444d6674980b62af0d052041ab57e4009ceceff97db568bb262` | `STATIC_READY_NO_PROVIDER_CALLS` |

Both identities lock `standard_cyber_safeguards=true`, EasyCLI `gpt-6-luna` Responses/max, the same task package and Debian 12 image, and the exact same host relay source. The only production ELF difference is the Context headroom budget fix. The relay also records the response's selected cyber program when the server includes that field; TB50 did not capture that metadata.

## Proposed limits and stop rule

Each slot is limited to 56 model decisions, 70 upstream attempts, 8,000,000 reserved input tokens, 440,000 output tokens, 1,400 tool attempts, and 8,400 seconds. The two-slot total is at most 112 decisions, 140 upstream attempts, 16,000,000 input tokens, 880,000 output tokens, and 2,800 tool attempts. The Plus membership quota remains the account-side ceiling; the relay cannot enforce or verify the account's remaining quota. The separate `$4` cap is only a conservative direct-API-equivalent estimate, not a Plus bill or hard account quota cap.

TB52 starts only if TB51 has a completed Runtime trial, an official Harbor grade, no Core denial, and no unknown usage. Any Daybreak verification error, other unknown usage, limit stop, or noncomparable first slot stops the sequence. Unknown reservations remain frozen and are never replayed. No old TB46/TB48 request is included.

The proposed Core grant candidate is a byte-for-byte copy of the prior 10-rule candidate: SHA-256 `0644d7b8628fa2478dce072d656ce726f5ba97b882cbff926d3a22b840584813`, expires **2026-09-30 02:00 Asia/Shanghai**, and retains the same permission scopes and counts. It has not been installed into a running Runtime or used for these new identities. It is available at [the candidate grant file](../../../scripts/terminal_bench_pilot/grants_tb51_tb52_standard_candidate.json).

Both full static identities, including binary hashes, grant hash, image ABI, task lock, relay source hash, and the explicit standard-mode flag, are in [the zero-provider preflight](TB51_TB52_STANDARD_STATIC_PREFLIGHT_20260929.json). The run requires a fresh explicit authorization; TB50 approval covered one diagnostic request only.
