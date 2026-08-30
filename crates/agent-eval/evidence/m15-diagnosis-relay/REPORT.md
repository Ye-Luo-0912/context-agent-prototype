# Relay-tuple M15 v4 window failure diagnosis (2026-08-30)

Post-window evidence analysis of the second formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788105967425`), following
the M15_ACCEPTANCE §5 rule 4 return-to-diagnosis from its valid FAIL (10/12).
Serving tuple: localhost OpenCode relay (`http://127.0.0.1:8787/v1`),
`deepseek-v4-flash`, Responses, 128,000 context, 16,384 max output tokens.
All facts below are read from the immutable per-cell event streams and the
harness `verify.json` records; no new live run produced them.

Verdict shape: 10/12 pass, 0 NOT_RUN, all cells provider healthy. Diag 4/4
and migrate 4/4; both `retry_policy_dev` failures (normal r2, resume r2)
share one mechanism: the model emitted a single response at or beyond the
pinned 16,384 max output tokens, and the provider failed closed.

## 1. retry_policy_dev normal r2: explicit output-limit error (1 cell)

Cell `retry_policy_dev-normal/r2-attempt8` (run `5beb660a...`): 5 model
rounds of small bounded reads and a `capability.manage` search (largest
single output 3,458 tokens), then the round-6 response exceeded
`max_output_tokens` and the run ended with
`error_class=model_output_limit` /
`model output limit reached: max_output_tokens` (`events.jsonl` seq 103).
No mutation had occurred; all calls were read-only exploration
(`fs.list`, `fs.read`, `git.status`, `search.grep`, `capability.manage`).

## 2. retry_policy_dev resume r2: truncated list-typed tool argument (1 cell)

Cell `retry_policy_dev-resume/r2-attempt8` (run `fccc501b...`): after
reading all four fixture sources (rounds 1-3), the round-4 response
contained at least three tool calls at output index 2 with a list-typed
argument whose JSON was truncated mid-array at byte ~7,921 (column 7922,
"EOF while parsing a list"). The provider's strict tool-call parser
rejected it fail-closed:
`error_class=model`, `model protocol error (malformed-tool-call): Responses
tool call at output index 2 has incomplete or invalid arguments: EOF while
parsing a list at line 1 column 7922` (`events.jsonl` seq 75-77). The
truncated call was never dispatched; the response stream had crossed the
16,384 output cap before the arguments completed.

## 3. Mechanism

Both failures are the same model-behavior pattern: `deepseek-v4-flash`
occasionally packs its whole next step into one oversized single response
(the ~7.9 KB argument list is the largest single tool-call payload
observed in this run), and the pinned 16,384 max output tokens cuts the
stream. The 4,096-token preflight attempt had the same shape and was the
reason the cap was raised to 16,384; the window shows 16,384 is still not
enough for this model's worst case. The provider behaved as designed in
both cells: the output cap is part of the pinned serving identity, and
malformed/truncated tool arguments fail closed instead of dispatching
partial work. No harness, transport, fixture or oracle defect is present.

## 4. Verdict

Per M15_ACCEPTANCE §5 rule 4 the valid FAIL (10/12, 0 NOT_RUN) rejects the
relay serving tuple as pinned (including the 16,384 max output tokens) and
returns to diagnosis; the window is not rerun. The string of relay
preflight failures (Chat SSE shape, 4,096-token truncation, wire-name
collision) plus these two output-cap failures is a model/tuple-behavior
pattern, not a harness defect. Candidate selection is a user decision.