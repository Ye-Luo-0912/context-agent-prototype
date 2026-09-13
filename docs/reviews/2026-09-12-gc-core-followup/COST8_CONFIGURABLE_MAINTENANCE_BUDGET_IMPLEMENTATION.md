# COST-8 实施回执：真正可配置的执行段维护预算

日期：2026-09-13。基线 `685b6bbb` 加共享未提交树；C 线第三片。

## 用户结果

用户能从产品入口（宿主/TUI/CLI 的进程环境）限制长期任务的压缩支出与同源失败重试：count/token 预算、失败退避真正接到引擎的每次维护；零预算不发送；预算的生效值在启动横幅一行可核对——`u64::MAX` 默认不再能被写成「总费用已受控」。

## 修复内容

1. **产品面预算配置（`agent-compose/src/lib.rs`）**：新 `MaintenanceBudget { max_calls_per_maintain, max_tokens_per_maintain, compact_failure_backoff_maintains }`（默认 4 / u64::MAX / 4），`maintenance_budget_from_env()` 严格解析三个环境变量（`MAINTENANCE_MAX_CALLS_PER_MAINTAIN`、`MAINTENANCE_MAX_TOKENS_PER_MAINTAIN`、`MAINTENANCE_COMPACT_FAILURE_BACKOFF`；未设保持默认，解析失败即启动错误，绝不静默回退）。文档逐条命名边界：两条预算都是**单次 maintain pass** 的上界（count=一次维护的串行压缩器调用数；token=一次维护的 in＋out 累计，observed/estimated），**不声称整个执行段的总费用上限**——分开的每次维护各自再花一份预算。
2. **配置真实接线（`build_context_engine` 第 5 参）**：预算值进入 Rolling 引擎配置（`RollingConfig.max_compactor_calls_per_maintain`/`max_compactor_tokens_per_maintain`/新字段）；**零预算（calls=0 或 tokens=0）完全不挂压缩器——零预算不发送**，引擎退化为纯 rolling（语义不变、无模型调用）。四个入口同线：宿主 `agent-host/src/main.rs`（横幅打印预算行）、TUI `main.rs`/`cli.rs`/`session.rs`。
3. **可核对的说明**：`MaintenanceBudget::describe()` 输出无密钥一行（「≤ N 次压缩器调用与 ≤ T token 每次维护；失败折叠退避 N 轮」；tokens 无界时如实写 `unbounded`），宿主启动横幅打印——运行的花费上限可核对，不是口口相传。
4. **同源失败退避（`context-baselines/src/rolling.rs`）**：新 `RollingConfig.compact_failure_backoff_maintains`（默认 4，0=每次触发都重试的旧行为）。失败的折叠请求以**候选身份摘要**（旧摘要 id＋候选记录 id 的哈希）登记；相同请求在其后 N 个「可折叠且轮到它」的维护触发上**延期**（`deferred_folds` 如实报告，不再打同一请求）；**折叠内容变化（新记录/新摘要/残余尾变化）使退避立即失效并重试**——复用随原文失效，不随日历。成功折叠清除退避；退避状态随 RollingState checkpoint 持久（serde default，旧检查点兼容），冷恢复后语义保持。预算判定仍在候选取走之前（W04/COST-4 语义），延期计数诚实。

## 回归

- baselines：`a_failed_fold_request_backs_off_until_the_content_changes`——失败入账→同内容下一次维护推迟（零新调用、deferred_folds>0）→内容变化立即重试（先在无退避代码上复现「每次触发重打同一请求」的反例）；`the_fold_backoff_survives_a_cold_restore`——退避状态随 checkpoint 走（`last_failed_fold` 在快照中可见），恢复后第一次维护仍推迟同一失败请求，验收清单「冷恢复后的预算语义」闭合。
- compose：`the_maintenance_budget_flows_into_the_engine_config`——预算描述行精确、零预算 `allows_calls()==false` 且引擎无压缩器仍正常工作；`maintenance_budget_env_parses_strictly`——未设即默认（默认描述如实含 `unbounded`）、显式值解析、非法值启动失败（含 `-1`）。

## 验证（实际命令与结果）

- `cargo test -p context-baselines --lib`：**23/23**（含本片 3 项）。
- `cargo test -p agent-compose --lib`：相干窗口 **38/38 全绿**（含本片两项：预算描述精确＋零预算不挂压缩器、env 严格解析含非法值启动失败）；`cargo test -p agent-runtime --lib` 相干窗口 **407/407**。host/TUI `cargo check` 0 错误。compose 全部 12 个测试目标与 runtime `--test actor` 86/86 在后续相干窗口全绿（live_walk/m16_restore/core3_restore_snapshot_paging 的 `build_context_engine` 调用点随新签名补预算参数）。
- `cargo fmt --check` 十 crate 通过；本片自有 crate clippy 0 警告。

## 边界与未验收（如实记录）

- **保留真实边界**：count/token 预算是单次维护的上界，不是执行段总费用；横幅与文档都不写「总费用已受控」。请求级输出 cap（COST-4）与独立维护 timeout（COST-4）复用未重做。
- retry 预算继承既有有界默认（重试次数在 transport 层有硬界），未新增独立 retry 预算配置——同源失败的重试浪费由本片退避收口。
- 轻量模型切换不实施（按任务书：只在语义回归与明确配置后考虑，不擅自改用户主模型）。
- 「额度用尽时 Agent 可恢复地让出」的模型可读呈现依赖 CTX-4 的 required-miss 渲染通道；本片只做预算生效与诚实延期报告，未新增让出文案。
- 未提交、未推送、未跑远端 CI；真实收益对照照旧 NOT_RUN（COST-5）。D04 同型：预算生效是确定性削减，收益声明归 COST-5 实测。
