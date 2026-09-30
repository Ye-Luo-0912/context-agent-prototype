# TB44/TB45 EasyCLI 408 重试验证（2026-09-29）

## 结果

TB44 使用用户批准的 grant 候选和 56 决策配对参数启动，但在 Harbor agent setup 的 `agent-tui --help` 检查处失败。容器错误为 `/lib/x86_64-linux-gnu/libc.so.6: version 'GLIBC_2.39' not found`；当前候选 ELF 是从 Ubuntu 2.39 构建，而固定 Terminal-Bench Debian 12 环境提供 glibc 2.36。TB44 的 relay receipt 显示 attempts=0，未联系 EasyCLI、没有模型请求，也未开始 Dynamic/ Rolling 的运行窗口。该启动失败保留在独立 artifacts，不覆盖。

将 ELF 改在已缓存的 `rust:1.97-bookworm` 构建容器中重建（Debian 12 / glibc 2.36），Rust 与 Cargo 均为 1.97.1；使用工作树 Cargo 锁文件和本地 registry cache，Docker 构建网络关闭。新 ELF SHA-256 为 `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d`。将 ELF 只读挂载进锁定的 Terminal-Bench 题目镜像后，`agent-tui --help` 和 `--state-dir` 检查通过；TB45 静态预检为 `STATIC_READY_NO_PROVIDER_CALLS`。

TB45 使用新 identity `TB45_EASYCLI_408_RETRY_56_PAIR_20260929`，仍使用同一份用户批准的 10 条 Core grant、相同路径/风险/次数/内容上限和到期时间，并保持相同任务、镜像、账户 quota 约束和本地 relay caps。每槽 56 决策、70 provider attempts、8M input、440k output、1400 tool attempts、8400 秒；Responses、`gpt-6-luna`、reasoning `max`，EasyCLI relay/client timeout 600/660 秒，HTTP 408 retry limit=1。

| 槽 | Runtime / Harbor | 上游账本 | 评分 |
| --- | --- | --- | --- |
| Dynamic | 56/56 决策，Runtime exit 0，`task_completed=false`，无 Core deny，109 次工具调用 | 56/56 completed；input 1,913,053、cached input 88,576、output 257,789；HTTP 408=0、retry=0、unknown=0 | official reward 0；可比单槽 |
| Rolling | 56/56 决策，Runtime exit 0，`task_completed=false`，无 Core deny，142 次工具调用 | 56/56 completed；input 2,356,598、cached input 0、output 186,347；HTTP 408=0、retry=0、unknown=0 | official reward 0；CTRF 15/18（3 失败）；可比单槽 |

两槽合计 112/112 次上游请求 completed，input 4,269,651、cached input 88,576、output 444,136，unknown=0、预算拒绝=0、Core 拒绝=0，配对比较 `COMPARABLE`。按当天 GPT-6 Luna OpenAI 直接 API 价格算出的账本等价值为 Dynamic `$0.31222796`、Rolling `$0.32883330`、合计 `$0.64106126`；这是 provider 价格模型估值，不是 EasyCLI Codex Plus 的实际账单或 quota 消耗数据。

TB43 原本发生在 Rolling 第 16 次、约 600 秒后的 HTTP 408 没有在 TB45 复现。两个 TB45 relay receipt 都记录 `retryable_408_retries_used=0`、`retryable_408_recovered=false`、`budget_unknown=false`。因此 TB45 验证了修复后 ELF 能在真实 Debian 12 task 里完成双槽，并验证了通常请求与记账路径；**没有验证真实供应商 408 后的重试能否恢复**。重试机制的现有证据仍是 localhost 合成回归，未来需等待自然发生的 408 或另行使用零供应商端到端注入覆盖该分支。

## 身份与限制

- 候选授权：`grants_tb44_retry_candidate.json`，10 条 grant 规则仅延至 2026-09-29 13:00（Asia/Shanghai），SHA-256 `45e12784db437e5bef5581136a545ef410411528e228089d93877e3b7ea70680`。TB45 在该已批准窗口内完成，没有新增权限或额度。
- TB44 artifacts：`/home/ye_luo/.cache/context-agent-terminal-bench/tb44-easycli-retry-pair-56/`；Harbor job 在同级 `jobs/` 下。
- TB45 artifacts：`/home/ye_luo/.cache/context-agent-terminal-bench/tb45-easycli-retry-pair-56/`；Harbor jobs 在同级 `jobs/` 下。
- 账户 Plus 实际 quota 使用/账单没有由 relay 报告；未知 request 在本试验为零，但 direct API 价格估算不能当作 subscription 消耗。
- 两槽官方 reward 都为 0，任务均未完成；用户先前关心的跨 grant 多文件 patch 场景是否实际被模型调用未在本回执中确认。
- TB43 那一笔 usage unknown 请求保持原状，没有重放。
