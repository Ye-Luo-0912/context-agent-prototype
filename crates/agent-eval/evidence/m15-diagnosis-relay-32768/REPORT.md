# 32,768-tuple M15 v4 window failure diagnosis (2026-08-30)

Post-window evidence analysis of the third formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788109477415`), following
the M15_ACCEPTANCE §5 rule 4 return-to-diagnosis from its valid FAIL (9/12).
Serving tuple: localhost OpenCode relay (`http://127.0.0.1:8787/v1`),
`deepseek-v4-flash`, Responses, 128,000 context, 32,768 max output tokens.
All facts are read from the immutable per-cell event streams and the
harness `verify.json` records; no new live run produced them.

Verdict shape: 9/12 pass, 0 NOT_RUN, all cells provider healthy. Diag 3/4,
migrate 4/4, policy 2/4. All three failures are
`error_class=model` / `malformed-tool-call`; none is an output-limit
failure.

## 1. The three failing cells

| cell | round | error detail |
| --- | --- | --- |
| `retry_diag_dev-normal/r1-attempt9` | 7 | `Responses tool call at output index 1 has incomplete or invalid arguments: expected ',' or '}' at line 1 column 521` |
| `retry_policy_dev-normal/r1-attempt9` | 13 | `Responses tool call at output index 2 has incomplete or invalid arguments: EOF while parsing a list at line 1 column 10526` |
| `retry_policy_dev-resume/r1-attempt9` | 6 | `Responses tool call at output index 3 has incomplete or invalid arguments: EOF while parsing a list at line 1 column 10736` |

The provider's strict tool-call parser rejected all three fail-closed; the
malformed calls were never dispatched. The failing argument columns (521,
10,526, 10,736 characters) sit far below the pinned output cap: 521
characters is ~130 tokens and ~10.5 KB is ~2,600 tokens versus 32,768 (and
even 16,384) tokens. None of these failures is an output-cap truncation.

## 2. Mechanism and correction of the earlier attribution

The 16,384-window diagnosis
(`crates/agent-eval/evidence/m15-diagnosis-relay/REPORT.md`) attributed
both of its policy failures to the pinned output cap. This window corrects
that: of the two 16,384-window failures, only `policy normal r2`
(`model_output_limit`, an explicit cap error) was cap-bound. The other
(and all three failures here) is a premature end of or syntax error in the
model-emitted tool-call argument JSON, at columns far below either cap.
No `model_output_limit` occurred anywhere in this window.

The mechanism is a model/serving wire-quality weakness: `deepseek-v4-flash`
served through this relay intermittently emits tool-call arguments that
end mid-list (`EOF while parsing a list`) or violate JSON syntax
(`expected ',' or '}'`), and the provider correctly fails closed rather
than dispatching partial work.

## 3. Stochasticity

The per-cell failures are not deterministic. Policy normal r2 and resume
r2 passed with `closure=completed` at 32,768 (they failed at 16,384),
while policy normal r1 and resume r1 failed here (they passed at 16,384).
Diag normal r1 flipped from pass to fail on a 521-character malformed
argument. The same pack/tuple combination produces different failing
cells across windows; the failure rate (2-3 cells per 12) is the serving's
limit, not any single cell's difficulty.

## 4. Verdict

Per M15_ACCEPTANCE §5 rule 4 the valid FAIL (9/12, 0 NOT_RUN) rejects the
32,768 relay tuple and returns to diagnosis; the window is not rerun. No
harness, transport, fixture or oracle defect is present. The relay line has
now produced three valid-FAIL windows (including this one) with two
distinct model-side mechanisms (explicit output-limit once; malformed
tool-call argument JSON three times). Candidate selection is a user
decision.