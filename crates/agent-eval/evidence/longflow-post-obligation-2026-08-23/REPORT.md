# Longflow after the Trust & Obligation program (2026-08-23)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com), repeats=2 on a clean tree
(git `ed2ddda`). All four arm-runs completed and passed the hidden
verification (file_content + command, 4/4 asserts each).

This is the first run carrying the second-round machinery end to end:
the Obligation Ledger (`resolution_fingerprint` preconditions),
capability-output metadata sanitizing, the user-role restored-body
frame, real `ArgumentDigest` evidence identity, and the
`ProtocolBodyCacheStats` event accounting.

## Results

| metric | C r1 | A r1 | C r2 | A r2 |
| --- | --- | --- | --- | --- |
| passed | ✅ | ✅ | ✅ | ✅ |
| wall_ms | 434903 | 437071 | 421175 | 285708 |
| rounds | 61 | 61 | 64 | 47 |
| tool calls | 80 | 60 | 78 | 49 |
| model input tokens | 515625 | 731818 | 543075 | 553676 |
| frontier_advances | 50 | 40 | 40 | 32 |
| redundant_evidence_calls | 1 | 1 | 1 | 3 |
| no_advance_peak | 3 | 5 | 6 | 3 |
| evidence_invalidations | 32 | 19 | 28 | 15 |
| fs.read motive proto-checkpoint-missing | 5 | 0 | 5 | 0 |

New cache counters (per cell, event-stream sums):

| counter | C r1 | A r1 | C r2 | A r2 |
| --- | --- | --- | --- | --- |
| protocol_cache_eligible | 31 | 30 | 30 | 20 |
| protocol_cache_hit | **0** | **0** | **0** | **0** |
| protocol_cache_miss | 31 | 30 | 30 | 20 |
| protocol_cache_invalidated | 16 | 10 | 13 | 6 |
| protocol_cache_oversize / restored_body_tokens | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |

## Finding 1 — the body cache never hits in command-heavy runs

Eligibility is high (20–31 offered rows per cell) yet zero rehydrations.
The mechanism is visible in the counters plus the tool mix: every
`process.run` / `shell.exec` has an Unknown mutation footprint, and each
one invalidates the *whole* turn cache (`protocol_cache_invalidated`
6–16 per cell; C arms ran 16–23 command tools). The conservative
policy is correctness-first and stays untouched under freeze — but it
starves the cache in exactly the workloads the cache was built for.
This is precisely what item-17 instrumentation was meant to expose;
any policy change is future work with its own evidence, not a retune
here.

## Finding 2 — guessing chains persist; the ledger's resolution rule is too coarse

Both C repeats rebuilt executable-guessing chains this time (previous
clean run: one of four arm-runs):

- C r1 fingerprint groups: `ed466dba`×2, `33d96c50`×2, `d12bdb18`×3
  (10 failed launches of 15 calls).
- C r2 groups: `494a003c`×2, `b6a710a5`×2, `f78b6ab6`×4 (9 failed of
  20 calls) — including two more guesses of the same binary *after* an
  intervening successful rebuild.

Within a chain the fingerprint stays stable, confirming the host-trusted
stamp works. Attempts still never escalate, for two structural reasons
visible in the traces:

1. Any successful command run resolves *all* ExecutableResolution
   obligations ("same-domain success"). A successful `rustc --test …`
   build clears the pending "compiled tests exe not found" blocker even
   though nothing about that blocker's precondition changed.
2. A successful build writes new files, so the *next* failed launch
   carries a different cwd listing → a new fingerprint supersedes the
   old obligation instead of accumulating attempts.

Refined CONV-03 residual: resolution must require a
precondition-matched success (same resolution fingerprint), not
domain-any-success, or the ledger cannot hold debt across
build-then-run cycles. Also noted: obligation warnings render into TASK
PROGRESS only and are not event-visible, so bundles cannot yet prove
whether warnings fired (small observability gap).

## Other readings

- `redundant_evidence_calls` collapsed to 1/1/1/3 (previous run 8/9/4/7):
  Runtime `ArgumentDigest` evidence identity stops collapsing
  same-tool/different-argument calls into false redundancy.
- `evidence_invalidations` rose to 15–32 (previous 0–11): the tightened
  currentness predicate retires superseded Resource rows immediately
  instead of letting them linger until the next observation — expected
  behavior, not extra world churn.
- Rounds/tokens are mixed against the previous clean n=2 (A worse,
  C similar/better); trajectory stochasticity at n=2 dominates and no
  performance delta is claimed.

## Verdict

All four cells pass hidden verification with the trust & obligation
machinery active. The run's value is diagnostic: cache hit rate and
obligation behavior are now measurable facts instead of inferences.
CONV-03's open scope is narrowed and sharpened (fingerprint-matched
resolution); the body-cache starvation is recorded as an observed fact
under freeze discipline. M12/M13 remain the mainline and stay unclosed.
