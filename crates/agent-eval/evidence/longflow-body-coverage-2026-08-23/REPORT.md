# Longflow after exact body coverage / evidence reconfirmation (2026-08-23)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com), repeats=1, production tool surface.
Both arms completed and passed hidden verification (file_content + command,
4/4 assertions each). The run used the dirty source tree identified by
`source_tree_digest=ada17785483161c75bcfd63fbffd2049207d67bfc564d0fd778d2a2214d41128`;
it is a development diagnostic, not a release gate.

## Results

| metric | C (dynamic) | A (append) |
| --- | ---: | ---: |
| passed | yes | yes |
| wall_ms | 471596 | 295833 |
| rounds | 51 | 40 |
| tool calls | 64 | 36 |
| model input tokens | 434171 | 420554 |
| model output tokens | 12083 | 6447 |
| provider tokens total | >=481775 | 427001 |
| historical-context tokens | 46325 | 114078 |
| turn-frame tokens | 55281 | 16831 |
| schema tokens | 55120 | 37256 |
| repeated / recovery `fs.read` | 12 / 12 | 11 / 11 |
| frontier advances / no-advance peak | 36 / 3 | 25 / 3 |
| reconfirmed evidence calls | 4 | 1 |
| evidence invalidations | 30 | 9 |

C had one provider retry. `usage_incomplete=false`, but
`provider_tokens_lower_bound=true`, so the failed attempt's provider cost is
not a complete token sample. Model-input and prompt-layer counters are still
complete for the 51 used rounds; no claim about exact total provider cost is
made from this cell.

## Comparison with the preceding C evidence

The preceding post-resolver C cells were 58/73 rounds, 71/109 tool calls,
and 487292/656211 model-input tokens. Against their two-cell mean, this cell
is lower by 22% in rounds (65.5 -> 51), 29% in tool calls (90 -> 64), and 24%
in used-round model input (571752 -> 434171). It is also below the better of
those two cells on all three counts. Historical-context tokens fell 37%
against the preceding C mean.

The paired C-over-A gap narrowed from the preceding aggregate +51% rounds,
+112% calls, and +23% model input to +28% rounds, +78% calls, and +3% model
input in this run. Wall time did not improve: C remained +59% over A. With
`n=1` and a stochastic model this is directional evidence, not a stable
effect estimate.

## What the trace supports

- Success was preserved without removing model-visible tools, reducing the
  round cap, adding a forced-completion prompt, or weakening Unknown-footprint
  invalidation. Edit calls and first-class mutation behavior remained on the
  production surface; both arms made the same 5 successful `edit.patch` and
  2 `fs.write` calls.
- Exact body coverage prevented TaskProgress identity from authorizing body
  omission by itself. The C prompt spent substantially fewer historical
  tokens than both preceding C cells while retaining file bodies when no
  other request layer carried them.
- Four same-result observations repaired currentness as
  `EvidenceReconfirmed`; they stayed visible to diagnostics but did not reset
  convergence debt as new evidence.
- Checkpoint-body restoration did **not** activate in this trajectory
  (`eligible=hit=restored_body_tokens=0`). One reread was still classified as
  `protocol_checkpoint_body_missing`. Therefore this run does not validate a
  cache-hit claim, and the observed reduction cannot be attributed to
  restoration.

## Context advantage remains

C still kept the context plane materially lighter than A: historical-context
tokens were 46325 versus 114078 (-59%), selected resident/reactivated tokens
were 23791 versus 82160 (-71%), and final resident bytes were 6498 versus
22399 (-71%). Per used model round, total model input was 8513 versus 10514
(-19%). The advantage was consumed at the task level by 11 extra rounds:
C's accumulated TurnFrame was 55281 tokens versus A's 16831 and tool-schema
tokens were 55120 versus 37256. Consequently total used-round model input was
still +3% for C. The next optimization must target execution/TurnFrame/schema
amplification; changing Context selection, GC, residency, or budgets is not
justified by this evidence.

## Residual call gap

C's 28 extra calls over A are now concentrated in generic exploration and
verification surfaces: `fs.list` +6, `search.grep` +5, `git.diff` +3,
`git.status` +3, and command execution +10 (`shell.exec` 11 versus A's one
`process.run`). `fs.read` differed by only +1; capability, edit, and write
counts were equal. The next optimization should consolidate already-covered
exploration/verification intent across equivalent tools, with evidence and
freshness as the key, rather than suppressing model initiative or adding a
benchmark-specific plan.

## Verdict

This is a successful first-stage correction: hidden success held, rounds and
calls moved materially downward, and used-round model input stayed bounded.
It does not close the convergence problem because C still exceeds A and the
single provider-total sample is a lower bound. Require at least one clean
repeat before treating the magnitude as stable. M12 and M13 remain unclosed.
