# CI 观察 — 固定 d3a05d29

- SHA：`d3a05d297295da1ece66b00245fa027f2ee12852`；提交时间2026-09-17 21:37:46 UTC / 2026-09-18 06:37:46 JST。
- [运行35278745998](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/35278745998)：attempt 1，completed / failure。
- 六个作业通过；`test (windows-latest, part full)` 失败，job id `105396528808`。
- 日志通过专用 `fetch_workflow_job_logs` 成功读取。以下是精确摘录，不是新执行结果。

```text
2026-09-17T22:00:18.7851231Z test production_trajectory_of_one_task_over_the_local_capture_server ... ok
2026-09-17T22:00:22.9501003Z test killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt ... ok
2026-09-17T22:01:52.6112653Z test named_pipe_t7_transient_failure_after_restore_retries_to_completion ... ok
2026-09-17T22:02:17.5978700Z test named_pipe_t7_same_task_full_backend_journey ... FAILED
2026-09-17T22:02:17.5987043Z thread 'named_pipe_t7_same_task_full_backend_journey' (6500) panicked at crates\agent-host\tests\host_t7_journey.rs:1366:6:
2026-09-17T22:02:17.5988374Z called `Result::unwrap()` on an `Err` value: the committed effect never landed for part_a.md must be a real workspace artifact before the correction
2026-09-17T22:02:17.5991565Z test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 33.15s
```

## 归因边界

确认的是同任务旅程在纠正前没有等到 `part_a.md` 的已提交效果。当前日志仅显示脚本round 0；不足以区分审批交付、工具准入、实际提交、事件消费或调度问题。
这不是本次的proof tree失败（该用例本次明确通过），也没有证据把它归因为H1–H5。
不要用延长超时、忽略测试、预先创建part_a.md或删掉磁盘断言来收口。

## 下一动作

在相同SHA和Windows配置下定向复现，保留approval request/response、OperationAccepted/ToolFinished、effect receipt与失败终态的身份关联。
用可控停点检查缺的是哪一步；恢复预期行为之后再跑完整旅程。定向命令示例（本环境未执行）：

```sh
cargo test -p agent-host --test host_t7_journey named_pipe_t7_same_task_full_backend_journey -- --exact --nocapture
```

详细测试启动/fixture条件沿仓库现有配置；该命令不含真实模型调用。
