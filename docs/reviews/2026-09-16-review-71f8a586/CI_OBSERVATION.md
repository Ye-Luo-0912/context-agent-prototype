# 远端 CI 观察（摘录，不是完整日志副本）

固定SHA：71f8a58614fcabbbc9bc9602fe85ef453df56765
Run：35156892711；attempt 1；completed / failure。
观察来源：GitHub.fetch workflow run 与 fetch_workflow_run_jobs、fetch_workflow_job_logs 的实际返回；没有在本地复现该Windows运行。

运行：https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/35156892711
失败job：https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/35156892711/job/104999146984

六项成功：document consistency；Linux check；Windows check；Linux test part1；Linux test part2；dotnet build/client tests。
失败：Windows test full（cargo test）。该命令因下面这个测试退出，不表示所有后续测试二进制已执行。

## 原日志相关行摘录

```text
2026-09-16T22:25:53.1030421Z running 1 test
2026-09-16T22:26:13.2998558Z test killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt ... FAILED
2026-09-16T22:26:13.3000415Z thread 'killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt' (5780) panicked at crates\agent-compose\tests\proof_supervision.rs:171:9:
2026-09-16T22:26:13.3001553Z timed out: exact proof tree exit (leader pid 8316 inspect Ok(Exited); member pid 3596 inspect Ok(Running(ProcessIdentity { pid: 3596, identity_token: "01dd462a55c4f564" })); host stderr: )
2026-09-16T22:26:13.3003814Z test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.20s
2026-09-16T22:26:13.3010451Z error: test failed, to rerun pass `-p agent-compose --test proof_supervision`
```

## 判断边界

此时 leader 退出但成员身份仍 Running，清理保证未通过。根因尚未由本审查定位，不将其无证据归为竞态、Job继承错误或负载抖动。Linux arm屏障与Windows containment实现不同，某一路通过不能替代另一路。

日志还显示 MCP cancel-during-write 用例通过；F5 指的是另一条 generic ProcessHost JSON-lines 发送路径，不能混为一谈。

现有 compose 缓存 smoke：`two_production_requests_of_one_task_share_the_key_and_carry_b0_b1` 通过。新 provider 变化矩阵未驱动实际Runtime，不意味着仓库完全没有生产链缓存测试。
