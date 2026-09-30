# TB-6 live closure-guard re-evaluation protocol

> 2026-09-22 纠正：`env` 实际获准执行；Harbor 在 CLI exit 3 后仍执行收集/验证。该槽 reward=0，验证在候选依赖安装失败，不能据此归为 PROTOCOL_INVALID。详见 [TB8 根因核查](TB8_ROOT_CAUSE_REPAIR_20260922.md)。

独立于 TB-3/TB-4 bounded pilot 和 TB5 live re-evaluation。针对 TB5 Rolling 过早调用 `task.complete` 的协议问题，固定加入 closure guard：官方 verifier 才是完成权威，受试 agent 不得调用 `task.complete`，必须继续工作并留下可验收 workspace。

- Task: `terminal-bench/live-database-cutover@4.0.0`
- Arms: Rolling → Dynamic
- Max rounds: 40 per arm
- Model: `deepseek-flash`, Chat non-thinking, output cap 8192
- Maintenance calls/tokens: 0/0
- Retries/concurrency: 0/1
- Old TB3/TB4/TB5 results remain frozen

## Result

Rolling was stopped as `PROTOCOL_INVALID`: after four dozen rounds of read-only probing, the model attempted ungranted `env` and `bash -lc pip download` calls. Core correctly denied them; the trial produced no verifier artifact. Dynamic was not started under this protocol. This result is retained as a harness boundary observation and does not alter previous scores.
