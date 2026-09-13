# M18 第三轮 B 线（EXEC-9 / EXEC-10 / EXEC-8 残余 / CTX-8 接线）实施回执

日期：2026-09-13。基线 `685b6bbb` 加共享未提交树（多会话并行）。本轮按 [第三轮任务书 B 份](TASK_B_EXECUTION_RECOVERY.md) 交付四片；上一轮 B 线五片（EXEC-5/6/7/8＋EXEC-4 残余）的回执见 [同目录上轮记录](../2026-09-12-exec-core-b-line/RECEIPT.md)。

## EXEC-9（R3-01/P1）：服务 Context 兑现保留检查点的正文保护

**用户结果：** 选择 service Context 后，较新状态清理存储，仍能从受保留的旧 checkpoint 读回正文。

- **wire**（context-contextcore/wire.rs）：新增三个协议操作——`checkpoint_recovery_item_ids { checkpoint }`（服务侧用引擎真实、版本化的根解析）、`storage_gc_protecting { roots, roots_complete }`、`reconcile_store_protecting { roots, roots_complete }`。
- **adapter**：三个 trait 方法改为走 wire；根解析失败是 `Err`（Runtime 记为「窗口不完整」→ 物理删除延期），不再是空集冒充「无保留」。
- **契约**：`ContextEngine::checkpoint_recovery_item_ids` 改 `async fn -> AgentResult<Vec<ContextItemId>>`（默认 `Ok(空)`＝「无检查点字节格式」的完整语义）；Simple 引擎实现同步改签名。服务 handle() 增三分支，把调用原样转给进程内引擎（真实解析在服务侧，版本随引擎 checkpoint 格式）。
- **枚举收敛**：actor 侧 `collect_checkpoint_recovery_roots` 与 spawned-boundary 的 `collect_checkpoint_recovery_roots_for` 收敛为后者的单一实现（actor 方法变成委托）；枚举失败→`complete=false`。
- **回归**（真实服务进程，tests/service.rs `recovery_roots_and_protected_reconcile_parity_across_the_service_boundary`）：外置→checkpoint A→Admit 回工作集→checkpoint B→restore B→A 根解析恰为 `[target]`（进程内/服务计数一致）；未知根命名→stale 重复仍删除（保护只覆盖命名根，诚实计数）；`roots_complete=false`→延期；保护性 reconcile→`deleted_stale=0`→restore A→正文逐字节读回。**红检查**：适配器临时退回空集时，测试在「A 必须命名外部正文」处失败（R3-01 缺陷形态）。

## EXEC-10（R3-09/P1＋R3-10/P2）：边界操作单槽准入与 restore 隔离

**用户结果：** 保存/显式完成/恢复期间得到明确受理或繁忙结果；每份回执可结束；恢复不续接旧终局事务。

- **单槽准入（R3-10）**：`spawn_gc_op` 顶部显式空位检查——槽位占用时返回 `Err((continuation, busy))`，把续体原样交还调用方；不再覆盖停靠条目（旧的覆盖会同时丢 reply sender 与 JoinHandle，两份请求都没有确定结局）。调用方语义：turn-scoped（turn-final/collect）busy 实际不可达，内联跑 pass 保住审计；checkpoint 维护 busy→**有界续接**（内联跑维护并立即落地，等待受引擎调用约束而非 store）；capture busy→内联采集（与旧行为一致的确定结果）。
- **restore 隔离（R3-09）**：`prepare_restore` 在安装任何状态前，若 `checkpoint_prepare` 停靠或 `gc_work` 持有 commit 类续体（TerminalFreeze），返回 typed busy 拒绝——旧 TaskTxn/prepared Context 永不对恢复后状态提交或回滚。拒绝是确定性、类型的；完成方结算（或重试）后 restore 可用。
- **既有 idle fence 保持**：`ensure_idle` 继续拒绝 mutation during commit-in-flight。
- **门控回归**（GatedBoundaryContext）：①`restore_is_refused_while_a_terminal_commit_is_parked`——终局冻结停靠时 restore 被类型化拒绝；放行后 completion 正常结算（completed==1、无活动任务、非 restored）。②`two_concurrent_checkpoint_captures_both_settle_deterministically`——并发两份 capture 都有确定回执（单槽＋有界内联续接），无丢失无覆盖。③既有 safe-point 停顿/显式 collect 停顿回归保持绿（Stop/Cancel 的有界性已覆盖）。

## EXEC-8 残余（R3-11/P2）：冷结果查询的读取工作和控制等待有界

**用户结果：** 长日志中查询退休任务结果时仍可取消/停止，journal 写入不被挤住。

- **读取边界（agent-storage）**：`read_trace_tail` 改为从文件末尾读一个**字节窗口**（`MAX_TAIL_SCAN_BYTES = 8 MiB`）——总扫描字节有硬上界，历史再长也不变；窗口未达文件头→`complete=false`（诚实不完整）；单行超过 `MAX_TAIL_LINE_BYTES = 1 MiB`→typed Storage 错误（fail-closed，绝不投喂超限行）。
- **writer 循环解锁**：`read_tail` 不再作为命令进入 journal writer 循环（旧实现整文件扫描会同时堵住 append/flush）——trait 实现先 `flush()`（与追加串行、快速），再在 `spawn_blocking` 任务里开只读句柄扫描。
- **Actor 分支解锁**：`TaskCompletionLookup` 的冷路径移到独立 tokio 任务——actor 只做热表检查与快照（runs/journal），spawn 后立即返回；reply 由任务发送。控制通道（status/cancel/stop）在任何扫描期间保持应答。
- **回归**：①环形窗口：200 行日志 max=50 → 恰 50 行最新、`complete=false`、首尾 seq 断言；②超大行 → typed 错误「single-row cap」。

## CTX-8 接线（R3-08/P2）：外置背压可观察、可恢复

**用户结果：** 持久存储故障时执行安全停在资源边界；修复 store 后可继续，已受理输入和已提交副作用仍可核对。

- **观察**：每个边界 pass（turn-final GC、完成边界、显式 collect）落地时，把 `ContextGcReport` 的 `externalize_backpressure / externalize_deferred / store_io_failures` 记入 actor 状态（`store_backpressure: Option<StoreBackpressure>`）；清一次 pass 即解除。
- **投影**：`RuntimeStatusSnapshot.store_backpressure: Option<StoreBackpressure{active, externalize_deferred, store_io_failures}>`——背压是可再取的快照事实（控制/查询通道全程可用）。
- **限制新正文生产**：背压激活期间，工具结果的 working-set 预热信号（best-effort 旁路）跳过；工具结果仍进回合帧（投递不依赖故障 store），用户输入与已完成效果不受影响；store 恢复后按批 drain 由引擎既有 pending/owned 机制承担（A 线 CTX-8 主体），Runtime 不建第二套 GC 权威。
- **回归**（BackpressureContext，gc 报告可编排）：背压 pass 后 status 显示 `active=true, deferred=7, io_failures=2`；第二个回合跑清洁 pass 后 `active=false`——同一快照通道先坏后好，全程可观测。

## 验证（本地 Windows，实际执行）

- `cargo test -p agent-runtime`：lib 407、actor 86、instance 4、work_control 32、shutdown 31、host_restore 3、turn **143** 全绿（含本轮全部新回归）。
- `cargo test -p agent-storage`：26 全绿（read_tail 窗口/超大行/环形回归）；`-p agent-context-service`：lib 10＋service 16（含 EXEC-9 服务链回归）；`-p agent-host`：22；`-p agent-tui`：60；`-p agent-compose`（m16 2/core3 1/kv 5）全绿。
- `dotnet test clients/dotnet/Agent.Client.Tests`：123/123。
- `cargo fmt`；`cargo clippy` B 线 crate 0 警告（context-simple/context-baselines 残余告警属并行 A 线文件）。

## 未验收与限制（如实记录）

- 全部改动未提交、未推送、未跑远端 CI。
- EXEC-9：服务根解析的「版本化」随引擎 checkpoint 格式自身版本走（Simple 引擎 serde 兼容）；未做长跑 soak。
- EXEC-10：restore 拒绝是确定性 busy——「完整结算后自动恢复」未做（调用方重试即可，语义等价）；busy 内联路径在门控测试下会阻塞 actor 至 gate 释放（真实引擎下有界）。
- EXEC-8 残余：字节窗口 8 MiB 为常量，未做成配置；超大行只会 fail-closed，不会截断服务。
- CTX-8 接线：限制点目前只覆盖 working-set 预热信号（投递路径不受影响）；「按批 drain 后解除背压」依赖引擎既有机制。共享树并行线（COST-7 的 `OperationOutcome::Failed.usage`、provider-openai 流式 usage 携带）编辑中间态由 B 线机械收敛。
- 本轮未调用付费模型；真实 provider 照旧 NOT_RUN。
