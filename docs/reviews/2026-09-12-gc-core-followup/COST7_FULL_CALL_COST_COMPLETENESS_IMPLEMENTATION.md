# COST-7 实施回执：全调用与所有终止路径的费用完整性（R2-11）

日期：2026-09-13。基线 `685b6bbb` 加共享未提交树；C 线第二片。

## 用户结果

模型或维护失败、取消、晚到时，账目说明已知费用与未知部分：失败的压缩调用不再从账本消失；空摘要的已计费调用保留 provider 已报的 usage；晚到/重复的完成一笔费用只入一次；维护 lane 的 unknown/估算/重试计入全局完整性——「只有维护有未知」的运行不再被读成完整账单。

## 修复内容（R2-11 四个断点）

1. **Dynamic「非零才入账」门槛移除（`context-simple/src/engine.rs`）**：ingest 蒸馏的 `pending_compactions.push` 不再要求 `input>0||output>0`——失败蒸馏的 Unknown 0/0 行进入账本，零用量的成功调用（Observed 零即事实）同样入账；身份字段向消费端区分两者。
2. **空摘要保留已计费 usage（`agent-compose/src/compactor.rs`＋`agent-contracts`）**：新类型化错误 `AgentError::EmptyCompactionSummary { usage: ModelUsage }`（附 `AgentError::reported_usage()` 提取助手）——空摘要仍以 Err 拒绝折叠（W08 语义不变：显示用 fallback 永不退役源正文），但 provider 已报的 usage 随错误交还引擎。Dynamic `run_distill` 与 baselines rolling 失败分支都改为：有类型化报告走真实计数与诚实身份（双计数 Observed、部分报告降级 Unknown＋已知值保留下界），无证据保持显式 Unknown 行——失败成本不再随 Err 消失。`CompactionOutput`/`ContextCompaction`/`ContextCompacted` 的 `cached_input_tokens` 改为 `Option<u64>`（缺失不再拍平成 0；COST-2 时代已写 0 的行按原字节解码为 `Some(0)`，不静默重解释），并增 `cache_write_input_tokens`/`cache_miss_input_tokens` Option 透传（serde default＋skip，旧行不受影响，金样字节稳定）。
3. **stale 结果用量补充与一笔一次（`agent-runtime/src/actor/{tools,turn,mod}.rs`）**：stale 完成的业务结果照旧丢弃，但——①stale MODEL 完成携带的 provider usage 以真实身份补发 `ModelUsed` 行；②stale MAINTENANCE 完成携带的引擎报告 compaction 行逐条补发 `ContextCompacted`；③去重栅栏 `usage_accounted_ops`（有界 64 FIFO，进程内）让「取消时已发 unknown 行＋晚到完成到达」与重复到达**只入账一次**。取消路径扩展：取消在飞 **Maintenance** op 现在也发一条 unknown 用量行（lane 标注 maintenance），不再只覆盖 OpKind::Model。
4. **lane 身份与全局完整性**：`RuntimeEvent::ModelUsed` 增 `role: ModelCallRole`（Main/Maintenance，serde default Main——legacy 行默认主 lane）。eval metrics：maintenance lane 独立计数（`maintenance_used_rows`/observed in+out），主调用账单不被压缩器行模糊；`ContextCompacted` 的 estimated/unknown 与 retries>0 现在置全局 `provider_tokens_lower_bound` 并计 `compaction_retries`——只在维护侧有未知/重试的运行如实读作「下界」。JSONL 重试观测增 per-writer `transport` 实例标签（`JsonlRetryObserver` 进程唯一实例号）：主/独立维护 transport 共享一个指标文件时，`(transport, call_seq)` 不再碰撞。

## 回归（红-first 或旧路径反例）

- compose compactor：空摘要错误携带 usage（`an_empty_summary_keeps_the_reported_usage_evidence`）、cache 三桶无损透传且缺席为 None。
- context-baselines：被拒折叠的已计费 usage 进账本（真实计数＋Observed，`a_refused_fold_keeps_the_billed_call_usage_in_the_ledger`）。
- context-simple：空摘要蒸馏保留已报 usage 进账本（`an_empty_summary_distill_keeps_the_reported_usage_in_the_ledger`）；无证据失败落显式 Unknown 行（`a_failed_distill_still_lands_an_unknown_row_in_the_ledger`——旧「非零才入账」门槛使该行整条消失，改动前必红）。
- runtime turn：取消＋晚到携带 usage 的完成**恰好一行**账目（unknown＋main lane），晚到结果不推进任务、不重复入账（`cancel_and_late_completion_count_one_cost_exactly_once`，先在旧代码上构造取消即丢/晚到即丢的两种丢失）。
- eval metrics：仅维护侧 unknown/重试即置全局下界并计重试（`maintenance_side_unknowns_and_retries_mark_the_bill_incomplete`）；maintenance lane 独立计数不混入主账单（`maintenance_lane_usage_rows_are_counted_apart_from_main_rounds`）。
- provider retry：共享指标文件上两个观察者实例的 transport 标签互异（`jsonl_observer_lines_carry_distinct_transport_tags`）。
- 协议跨语言：cache-fields 金样（COST-6 同一 fixture）同时钉 `role:"main"`。

## 验证（实际命令与结果）

- `cargo test -p context-simple --lib`：**351/351 全绿**（含 10k 轮长任务与新回归；并行负载下该测试偶发进程异常退出，单独重跑 120s 通过——既有负载抖动类型，非本片回归）
- `cargo test -p context-baselines --lib`：**22/22**；`cargo test -p agent-eval --bin agent-eval -- metrics::tests`：**31/31**；`cargo test -p agent-runtime --test turn -- stream::`：**6/6**（含新去重回归）
- `cargo test -p agent-runtime --lib`：相干窗口 **407/407 全绿**（此前的 406＋1 失败经核实为并行 EXEC-7 对 completion-sender 的在飞编辑，其窗口收口后全绿）
- `cargo test -p agent-runtime --test actor`：**86/86 全绿**；`cargo test -p agent-compose`（全部 12 个目标）：全绿（lib 38＋live_walk/m16_restore/core3_restore_snapshot_paging 等集成目标——三处 `build_context_engine` 调用点随本片新签名机械补 `&MaintenanceBudget::default()`）
- `cargo test -p agent-contracts --lib` **174**、`cargo test -p provider-openai` **129**、`cargo test -p agent-platform-protocol` **47＋14**、`dotnet test` 全量 **120/120**
- `cargo fmt --check`（十 crate）通过；本片自有 crate clippy 0 警告。
- 整仓一次性的 `cargo test --workspace` 在四路并行构建争用下无法在有限时间内完成（context_simple 套件被拖到 1 小时+，日志零失败行后中止）；每个 crate 的全量套件均已在独立窗口跑绿（contracts 174、provider 129、protocol 47＋14、baselines 23、context-simple 351、eval 225、runtime lib 407＋actor 86＋instance 4＋turn 32/31＋recall 3、host 8＋3＋3＋8、tui 58＋2、compose 38＋12 目标、dotnet 120）。全仓一次性运行归集成/CI 窗口（既有约定）。
- **共享树并行域（如实记录，不归属本片）**：并行 B 线（EXEC-7：GC/checkpoint 入 operation lane）与 A 线（CTX-5/6/8/9：anchor 保护、pending owner、stored metadata、GC 背压字段、scope 退休）在飞编辑；验证窗口间 agent-runtime/context-simple 反复出现非本片编译中间态，本片全部验证均在可编译窗口完成。COST-4 时已存在的 agent-eval 三个文件的 fmt 漂移由本片窗口的 crate 级 fmt 顺带归一（语义中性）；A 线新增 `ContextGcReport` 背压字段（externalize_deferred/backpressure/store_io_failures）的 eval 测试字面量由本片窗口机械适配。runtime 全量套件最终窗口：lib 407、actor 86、instance 4、turn/directive 等 32、turn/effects 31、recall 3 全绿；唯一失败 `completions_past_the_hot_window_keep_checkpoints_restore_and_next_completion_working`（turn 134/135）经归因属并行 B 线 EXEC-8 自己未提交的新测试（`git diff HEAD` 中 +140 行，非本片改动）在其在飞恢复重开流程的独占日志锁上失败——保留给该线收口。

## 未验收（如实记录）

- 未提交、未推送、未跑远端 CI。
- 维护取消 unknown 行（lane=maintenance）为 actor 侧与 E05.1 同型的 6 行扩展，未构造「维护 op 在飞时取消」的独立 actor 回归（gated 引擎挂起维护的测试基建属 EXEC-7 重构域）；其正确性由代码审查与 lane 归属测试间接覆盖，如实记录。
- GUI 数字「不只依赖当前连接收到的事件」的快照水位汇总范围断言未单列（GUI 账目随连接纪元重置为既有 C4 设计，跨重连汇总属 COST-5 联合验收的记录范围）。
- 真实 provider 的账单对照照旧 NOT_RUN（COST-5）。
