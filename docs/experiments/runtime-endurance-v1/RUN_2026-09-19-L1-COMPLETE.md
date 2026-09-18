# L1 campaign completion receipt

Status: **L1_COMPLETE_WITH_MANUAL_REPAIR**.

The real-model campaign reached a provider output-limit failure, recovered from
the same task checkpoint, applied independent feedback across several segments,
and then reached the final deterministic acceptance boundary. The model did not
finish every delivery item autonomously: the controller added the final
non-string JSON-key validation and corrected two malformed generated regression
tests after the model's last segment was blocked by `process.run` approval
denials. Those repairs are explicitly separated here from model evidence.

## Final acceptance

All tests below ran against the final isolated workspace under
`target/l1-durablequeue-post-f12-20260919/workspace`:

| Layer | Result |
| --- | --- |
| Immutable public suite | 25/25 passed |
| Generated focused regressions | 16/16 passed |
| Frozen independent reviewer | 18/18 passed |
| Four-process SQLite contention | passed inside independent reviewer |
| Crashed worker, lease expiry and stale token fencing | passed inside independent reviewer |
| Seeded 180-step state-machine oracle | passed inside independent reviewer |
| Future-schema refusal, pagination and atomic rollback | passed inside independent reviewer |
| Required `app/README.md` and `app/DESIGN.md` | present |

The fixed reviewer closes its SQLite connection before temporary-directory
cleanup, so the earlier Windows `WinError 32` was removed from the verdict. The
final independent run used no mutable fixture or public-test changes.

## Runtime campaign evidence

The campaign retained its original 56 real model requests and 88 tool attempts,
with a peak cost estimate of `$0.2852241` under the `$1.00` campaign cap. The
first segment stopped at `model_output_limit` after durable checkpoint sequences
1–3; the second segment cold-restored and completed the remaining task work.
Feedback segments corrected duplicate IDs, strict string validation, immutable
identity after retry, tuple payloads and finally non-string object keys.

The last feedback segment did not run its requested verification commands because
the model omitted the explicitly granted Python executable from `process.run`
argv. Core correctly rejected the effect-derived intent against the standing
grant and returned `approval_denied`. The controller then ran the deterministic
tests outside the model and recorded the final green counts above. The missing
process invocation is therefore a model/tool-use failure, not a permission
expansion or a hidden successful verification.

## Scope boundary

This completes the isolated DurableQueue L1 proxy campaign and the Runtime
failure/recovery gates. It does not claim that the larger planned incremental
data-build platform, 90-minute soak, or every F01–F20 combination has run. The
scenario manifest retains those as separate future coverage.

Evidence:

- Campaign segments: `target/l1-durablequeue-post-f12-20260919/l1-segment-1/`
  through `l1-segment-5-final/`
- Final application workspace: `target/l1-durablequeue-post-f12-20260919/workspace/`
- Fixed independent reviewer: `target/l1-durablequeue-post-f12-20260919/reviewer_fixed.py`
- Runtime repair receipt: [RUN_2026-09-18-FIXED.md](RUN_2026-09-18-FIXED.md)
