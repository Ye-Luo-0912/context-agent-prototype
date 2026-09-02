# M15 exact-source/product preflight

One `retry_policy_dev` normal cell on a clean HEAD with the product surface (TaskProgress on, settlement and all advisory candidates off), the pinned serving tuple and explicit protocol, before the single predeclared 12-cell window. Cell dir: `crates/agent-eval/evidence/m15-preflight\retry_policy_dev-normal\r1-attempt4\dynamic`.

| dimension | value |
|---|---|
| verdict | FAIL |
| cell verdict | not_run (behavior not_run, diff pass, closure failed, continuation n/a) |
| provider health | transport_failed |
| turns/task closed | false / false |
| model rounds | 1 |
| wall ms | 59625 |
| source tree digest | 83b47da50d71cb662b2ad2cc4f028e1ee54cf28b5b93a4d2b92101cd8af11bb0 |
| switches | task_progress=true settlement=false opportunity=false recovery=false diag=false |
| error | `phase one failed: transport error (retryable=true): request failed: error sending request for url (http://127.0.0.1:8787/v1/responses)` |

Verdict: FAIL
