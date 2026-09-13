# EXEC-1 实施回执：材料化等待可取消（M18 B 线首片）

基线：`685b6bbb` ＋ 既有未提交工作树（M17/M18 各线混合树，未从裸 HEAD 覆盖任何他人改动）。对应 [REPORT.md](REPORT.md) **E03（P1）**、[TASKS.md](TASKS.md) EXEC-1。本片在**工作树**落地（未提交、未推送、未跑远端 CI、未调用真实 provider）。

## 用户能获得什么

上下文读取慢（慢外存、慢引擎 gate）时，取消与停止命令仍被及时受理：用户拿到可信的 `TurnCancelled`（或存储失败时明确的 `RecoveryRequired` 围栏），已提交的指令与已落地的副作用保持不变；旧材料化操作的晚到预览不会复活任何已取消的回合。

## E03 复盘与本片修复

**缺陷**：模型轮准备（BeforeModel 维护完成后的续体）在 Actor 的 select 分支内**内联 await** `context_materialize`（`services.rs` 直接转发；真实 SimpleContextEngine 的 materialize 在引擎 gate＋外存读取处只受字节上限约束、无时间上限）。等待期间命令通道不被消费——Cancel/Stop 全部滞留。

**修复（沿 W04 已建立的 operation lane，不新增调度器）**：

- `OpKind::Materialize` 新操作类型。续体在构建完整 `ContextQuery`（全部克隆，Actor 只计划/核对代际/提交）后，把准备尾段打包为 `ModelRoundPlan`（turn_id、model_round、turn_frame、runtime_focus、task_view、base_progress_view、settlement_candidate、project_settlement、settlement_projection_diagnostics、materialize_started、output_reserve、send_window、surface_plan、proof_surface_available）驻留在 Actor（`state.materialization`），随 `spawn_materialization` 派生引擎调用。
- 完成项经既有 `OperationCompletion` 通道返回（`select!` biased on cancel——取消竞争时发送让位）；`on_operation_completed` 代际围栏（`is_stale`）通过后，`continue_model_operation_after_materialize` 以原名解构恢复尾段——尾段逐字节保持原语义（校验、shadow frame、组装、预算裁剪、事件、surface revision、provider spawn）。
- **abort 安全性依据**：`materialize` 是引擎文档化的非消费预览——abort 释放 gate/state 锁、无消费提交、无成本发生；事件时钟不前进。`cancel_turn` 新增 `cancel_pending_materialization`：既有 Core 代际围栏后，按既有 5 秒清理上限 join；**join 未确认则不声明可信取消**——置 `recovery_required`、发 `TurnCommitFailed{phase:"materialize_cancel_cleanup"}`＋`RecoveryRequired`，拒绝接纳新状态。stale 晚到完成项丢弃并随 `.materialization` 清理（非消费，丢弃即全部回滚）。
- `services.rs::context_materialize` 转发器（E03 引用的直接 await 路径）随之删除；Stop 路径经既有 `cancel_turn(Shutdown)` 自动获得同一有界清理。
- 非本片范围（按停止条件不动）：`revalidate_stored_resource_facts` 的 workspace stat 等未证实阻塞的异步路径。

## 回归（先红后绿）

actor 集成测试（`tests/actor/materialize_cancel.rs`，门控引擎 `GatedContextEngine`——materialize 停在 notify 屏障，全部时序确定性、无 sleep 猜测）：

1. `cancel_is_answered_while_materialization_is_parked`——**报告反例**。挂起时取消在看门狗内被受理（`TurnCancelled`），directive 任务保持活跃；释放后晚到预览不复活回合（无 `TurnCompleted`）。**红已验证**：临时以 `EXEC1_RED_CHECK=1` 切回内联 await（与旧代码逐语义等价），该测试在看门狗处失败。
2. `materialization_release_and_cancel_arriving_together_have_one_terminal`——释放与取消同到：两种竞速次序都恰好一个终态（Cancelled×1 或 Completed×1），运行时可继续使用。
3. `a_fresh_round_runs_after_a_parked_materialization_was_cancelled`——取消后新对话到达**新的** materialize 并完成（车道恢复）；旧轮晚到预览不执行。
4. `stop_with_a_parked_materialization_shuts_down_bounded`——挂起时 Stop 在既有清理上限内有界完成。
5. `materialization_storage_failure_fences_the_round`——存储失败 → `TurnCommitFailed{phase:"context_materialize"}`＋`RecoveryRequired`（围栏而非盲重试）。

真引擎回归（`context-simple/src/tests/foreground.rs`，既有 `IoBoundaryPause` 确定性屏障——真实 SimpleContextEngine 停在其存储读边界）：`an_aborted_materialize_wait_releases_the_engine_and_commits_nothing`——abort 有界结束；gate 释放（后续 materialize 立即可用）；事件时钟不动、无残留 pending 预览（唯一 pending 属于后续调用自己的预览）。中途一次断言写错（把后续调用的合法 pending 误判为残留）已修正——非被测代码缺陷。

配套：`restore_tests` 的完成项泵更新为跳过内部准备操作（Maintenance/**Materialize**）只认终态——拆分引入的新 op 序列所需。

## 实际执行的检查（本机 Windows，2026-09-12）

- `cargo test -p agent-runtime --test actor`：**86/86**（81＋新增 5）。
- `cargo test -p agent-runtime --lib`：**393/393**（泵修正后；此前 3 处 restore_tests 失败均为泵未跟上新 op 序列，已修）。
- `cargo test -p context-simple --lib`：**326 通过、1 失败**——`gc::full::tests::gc_never_resurrects_superseded_items`（及早前 4 个 lifecycle supersession 测试）属 **A 线 CTX-2 在飞改写**（`reachability.rs`/`lifecycle.rs` 正被并行会话编辑，失败测试名与任务书 CTX-2 回归名单一致；本片未触碰 supersession 语义，我的新增测试通过）。
- `cargo test -p context-baselines`：17 通过。`cargo test -p agent-compose`：全绿（含恢复/restore 走查）。
- `cargo test -p agent-runtime --test turn`：**1 失败（既有，非本片引入）**——`safepoint::failed_checkpoint_write_fences_continuation_until_a_retry_lands` 在新旧两条路径（含 `EXEC1_RED_CHECK=1` 复现旧内联行为）下**同样失败**，与 checkpoint/恢复域的树上并行改动相关，归 CORE/B-restore 后续处理，本片不背书。
- fmt：本片文件干净（工作树剩余 1 个 fmt diff 在 A 线在飞的 `reachability.rs`，不属本片）。clippy（agent-runtime＋context-simple，all-targets）：**0 警告**。

## 默认入口与边界

- 默认产品入口（compose→actor）自动获得本行为，无配置开关。
- Actor 仍是唯一编排者；Core 仍拥有 effect/提交权威；预览非消费的语义未变；未缩短任何回合或完成语义。
- **未验收（如实记录）**：未提交、未推送、未跑远端 CI；上述树上的 safepoint/supersession 失败属并行线在飞域，不在本片修复范围；真实 provider 与冷恢复全旅程照旧 NOT_RUN。
- **下一片**：EXEC-2（完成任务热投影有界）。
- **后续补记（2026-09-12，EXEC-4 验收期）**：本片重构的时序影响在 `turn::safepoint::failed_checkpoint_write_fences_continuation_until_a_retry_lands` 上显现——材料化 op 化多一次 operation-completion 往返，tool 批次失败写入先行结算，turn-1 finalize 多尝试一次诚实重试捕获（对阻塞 store 消耗一个序列号）。正确性保持（fence 成立、retry durable 语义不变）；该测试断言已从钉死 (1,2) 更新为语义不变量（revision 不变＋sequence 严格更新），turn 套件 133/133。归因与处置详见 EXEC-4 验收回执集成注记。
