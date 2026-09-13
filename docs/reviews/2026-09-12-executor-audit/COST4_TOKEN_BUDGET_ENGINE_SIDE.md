# COST-4 A 线后半：引擎侧单维护 token 预算——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 C 线 COST-4 的 A 线后半（COST-4 行明示「引擎侧单维护 token 预算（rolling/报表）属 A 线」）；承接首半（并行会话的请求级输出上限，[回执](COST4_MAINTENANCE_OUTPUT_CAP_IMPLEMENTATION.md)）
- 实现落点：`crates/context-baselines/src/rolling.rs`（预算判定）、`crates/agent-contracts/src/context.rs`（报表字段）、`crates/context-baselines/src/lib.rs`（2 项新回归）；无 wire 变更

## 用户能做什么

配置了维护 token 预算的长任务：一次维护内的压缩器花费（输入＋输出累计）到达预算即**明确延期**——剩余折叠计入既有 `deferred_folds`、报表置 `compaction_budget_exhausted`，由下一次维护继续消费；已折叠部分保留、源正文不被虚报。默认不配置（u64::MAX）行为逐字节不变。

## 语义

- `RollingConfig.max_compactor_tokens_per_maintain: u64`（默认 `u64::MAX`＝不限制，opt-in profile）。累计口径：压缩器调用的 input＋output 计数（observed/estimated）；**unknown 行不带数字**——其「是否发生调用」由 CALL 预算（W04 的 `max_compactor_calls_per_maintain`）覆盖，两个预算在报表上可区分（`compaction_budget_exhausted` 专指 token 门槛）。
- 判定位置：与 CALL 预算同点、都在 `take_fold_job` 把候选移出工作集**之前**——延期计数保持诚实（W04 反例语义复用）。输出长度事前不可知（provider 侧由首半的请求级 `max_output_tokens` 封顶）：一次调用越过剩余预算的部分照常入账并折叠，**下一次迭代**延期。
- 报表：`ContextMaintenanceReport.compaction_budget_exhausted: bool`（serde default false——旧行与无限预算不可区分于「全部消费」，不伪造）。

## 回归（2 项新增）

- `a_token_budget_defers_the_rest_of_the_pass_and_reports_it`：14 tokens/次、预算 20 → 恰 2 次调用后延期（calls==2、exhausted、deferred>0、累计 in=20/out=8 精确）。
- `the_default_token_budget_keeps_existing_behavior`（对照）：u64::MAX 下同一批候选只有 CALL 预算生效、exhausted 保持 false。

## 实际检查

- `cargo test -p context-baselines --lib`：**20 通过**（基线 17＋2＋1 项属性修复恢复）
- `cargo fmt`/`cargo clippy -p context-baselines --all-targets`：0 警告
- 取消贯通：折叠循环的取消/超时语义由 W04 既有机制承担，本片未改执行路径

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未调用真实 provider。
- COST-4 的 compose 产品面（独立 maintenance profile 的时间/重试/模型选择）归 C 线；「先同模型更小输出预算，再比较轻量模型」的质量对照归 COST-5 窗口。

## 下一步

M18 全部非条件切片至此代码落地（A：CTX-1..4；B：EXEC-1..4；C：COST-1/2/4＋COST-3 双半）。剩余：COST-3 provider 侧测量、COST-4 compose 产品面（C 线）、COST-5 阶段验收（真实 provider，条件项）。
