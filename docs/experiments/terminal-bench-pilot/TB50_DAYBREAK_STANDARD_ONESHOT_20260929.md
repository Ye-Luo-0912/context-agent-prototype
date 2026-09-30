# TB50 EasyCLI explicit standard-mode one-shot (2026-09-29)

After TB48 stopped on the backend Daybreak Blue verification 503, the user approved one fresh, tightly bounded request to test the standard-safeguard route. This was a connectivity/access-path probe, not a Terminal-Bench trial and not a continuation of TB49.

## Boundaries and preflight

- New identity: `TB50_EASYCLI_DAYBREAK_STANDARD_ONESHOT_20260929`.
- One upstream Responses attempt; explicit `access_programs.cyber="standard"`; 8,192 reserved-input cap; 16 output-token cap; zero retries; 60-second upstream timeout.
- The local `/v1/models` endpoint returned HTTP 200 but omitted the exact `gpt-6-luna` ID. This was recorded rather than treated as a denial because TB48 had already directly completed 18 requests on that same model and account route.
- The preflight itself made zero provider calls. It passed before the single request.

## Result

The request returned HTTP 200 and `response.completed`; the relay settled usage at 306 input tokens, 0 cached input tokens, and 16 output tokens. EasyCLI's read-only usage database independently recorded one successful `POST /v1/responses` row with the same usage and 2,250 ms latency. Relay state is `completed`; `budget_unknown=false`; retries used 0. No new error log was created.

This establishes that the local EasyCLI OAuth route completed one small Responses request when the client supplied the standard access-program field. It makes the TB48 Daybreak Blue verification path the leading explanation for that failure. It does not prove the backend applied `standard`: this one-shot did not capture the completion response's `access_programs` metadata, and the official [Responses API Daybreak guide](https://developers.openai.com/api/docs/guides/daybreak) does not specify the separate EasyCLI OAuth backend. The requested answer text was also not inspected. The 16-token output cap was reached, but the response still emitted `response.completed` and settled usage.

No Terminal-Bench score was produced. TB48 remains noncomparable and TB49 remains unstarted. Existing unknown reservations from TB46/TB48 were not replayed or cleared.

## Local relay preparation

The host relay now has an opt-in `standard_cyber_safeguards` setting for Responses traffic. It inserts or overrides `access_programs.cyber` as `standard`, preserves other request fields, includes the changed bytes in its reservation, and records a bounded `observed_cyber_program` field if the completed response returns that metadata. The bounded runner exposes this only through `--standard-cyber-safeguards`, restricts the option to the local EasyCLI Responses profile, and freezes it into the trial identity.

The synthetic relay regression verified that the outgoing request carries `standard` even when the incoming request asks for `daybreak_blue`, and that the relay records the response's selected program without retaining response text. The relay and pilot Python modules passed 41 tests (40 passed, 1 platform-specific skip). No additional live request was made while wiring this option.

Machine-readable result: [one-shot result](TB50_DAYBREAK_STANDARD_ONESHOT_20260929.json) and [relay receipt](TB50_DAYBREAK_STANDARD_RELAY_RECEIPT_20260929.json).

## Next step

To compare the old and updated ELF on the fixed Dynamic task under the now-explicit standard mode, create new trial identities and a new Core grant candidate, then run supplier-free preflight first. The previous TB48/TB49 authority was stopped by its unknown-usage condition; the TB50 approval covered one request only. A new two-slot evaluation therefore needs a fresh, explicit bounded authorization. Preserve the Plus-quota ceiling and stop immediately on another usage-unknown event or noncomparable arm.
