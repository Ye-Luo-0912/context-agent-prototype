# TB-5 live re-evaluation protocol

> 2026-09-22 纠正：下方历史解释不作为当前结论。TB5 R 的工具被 Runtime 完成停滞门提前关闭；TB7 有真实官方 2/18 结果，不能标为 PROTOCOL_INVALID。详见 [TB8 根因核查](TB8_ROOT_CAUSE_REPAIR_20260922.md)。原始实验文件和数值未覆盖。

日期：2026-09-22。该协议独立于 TB-3-A/TB-4-B bounded pilot，不修改旧成绩。

- Task：`terminal-bench/live-database-cutover@4.0.0`
- Arms：Rolling → Dynamic，顺序固定
- Model：`deepseek-flash`，Chat non-thinking
- Maintenance：0 calls / 0 tokens
- Max rounds：40 per arm
- Output cap：8192 tokens/request
- Context proposal：32768 tokens
- Retries：0；concurrency：1
- State：external `/tmp/context-agent-run/state`
- Grants：live task API scope plus bounded python3/pytest/ls/curl/env
- Stop: preserve each Harbor result, verifier output, artifact and usage; do not backfill the previous 20-round scores.

This is a real provider re-evaluation of the original task under a new protocol identity. The supplier-free fault variant that passed 18/18 remains separate evidence and is not injected into the task.

## Result

- Rolling: 4 model rounds, input 16,192 / output 1,132 / cached 5,376; `task.complete` was refused by OperatorClosureOnly; no verifier artifact; reward 0.
- Dynamic: 37 model rounds, input 468,326 / output 8,431 / cached 164,480; fresh MySQL-less verifier failed API boot on the unresolved `MYSQL_HOST` dependency; reward 0.
- Pair total: input 484,518 / output 9,563 / cached 169,856; off-peak local estimate about USD 0.0534; Harbor billing unavailable.

These results are a new protocol identity and do not alter the earlier 20-round bounded scores or the supplier-free fault-variant pass.

## TB7_READONLY_BROKER result

TB7 attempted to provide a bounded read-only broker through a mounted script, but the model emitted nonexistent tool names (`read_probe`, `fc_list`) and ungranted writes. Core correctly rejected them; Rolling is `PROTOCOL_INVALID`, Dynamic was not started. The protocol is closed without widening arbitrary shell or write authority.
