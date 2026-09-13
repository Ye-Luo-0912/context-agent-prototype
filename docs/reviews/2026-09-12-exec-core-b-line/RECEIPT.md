# M18 B 线（执行核心与恢复）五片实施回执

日期：2026-09-12。基线 `685b6bbb` 加未提交工作树（多会话共享树，含并行 A/COST 线在飞改动）。本轮按用户指示一次性交付 B 组全部五片：EXEC-5 / EXEC-6 / EXEC-7 / EXEC-8 / EXEC-4 残余。共享契约由 B 线合入；并行线（CTX/COST-6/7）的中间态编译缺口按共享契约合入职责做了机械修复（`ModelUsage.cache_miss_input_tokens` 字面量、`ModelUsed.role` 字面量、`ContextCompacted` 新字段的 fixture 模式与 `cached_input_tokens` Option 化断言），未触碰其语义域。

## EXEC-5（R2-01/P1）：热裁剪与 checkpoint 任务—完成记录一致性

**用户结果：** 完成第 65 个及之后任务仍能保存、完成下一任务并正式冷恢复。

- **修复**：`task.rs` 新增单一有界热投影 `bounded_hot_pair_window`（`HotTaskRow` trait 统一 `TaskRecord` 与 `TaskRecordSnapshot` 两侧行），完成任务行与其完成记录**成对淘汰**（最旧优先，键 `(last_active_ms, created_at_ms)`）；孤立回执一律投影出局（legacy 恢复防御）。`enforce_hot_bounds` 与 `prospective_terminal_snapshot` 共用同一投影——终局 checkpoint 承认的形状就是提交后内存持有的形状。`MAX_HOT_COMPLETION_RECORDS(256)` 降级为 legacy 上界（投影后 `completed.len() ≤ 行数 ≤ 64`，`debug_assert` 钉住）。
- **红-first**：7 项单测在旧代码上失败——含真实 `RuntimeCheckpoint::validate` 门（64/65 边界、prospective 一致性、legacy 100 对恢复、孤立回执丢弃、1000 次完成）。三个 EXEC-2 既有测试按成对语义改写（300 次完成 → records=64、evicted=236 等）。
- **全链回归**（真实 actor，非 TaskManager helper）：`completion::completions_past_the_hot_window_keep_checkpoints_restore_and_next_completion_working`——66 次 `set_focus`+`complete_current_task`，每次完成后 `checkpoint().validate()`；真实终端提交写下的 checkpoint 文件经 `decode_checkpoint_file` 解码+validate，冷恢复进新实例后再完成一任务再 validate。修复前该测试在第 66 次完成报错（孤立回执使 validate 拒绝）。

## EXEC-6（R2-02/P1）：typed 恢复引用、Unicode 安全与读授权

**用户结果：** 有效恢复保留必需旧产物；正文伪引用不增加读权限；坏引用不使恢复 panic。

- **红**：旧 `extract_protected_run_ids` 对报告原反例（`artifact://run/`＋35 个 `a`＋`汉`）实测 panic（字节切片越 char 边界，`restore.rs` 行 697）。
- **修复**：
  - `protected_runs_from_checkpoint`：只在 `decode_checkpoint_bytes`（含 validate）**之后**，从恢复 checkpoint 自身的 typed 字段（CompletionRecord 的 `artifacts`/`final_output_ref`、directive 的 `body_ref`）用 `ArtifactLocator::parse_sealed` 提取——真实 sealed locator 格式 `artifact://v1/<run>/<owner>/<digest>`。needle 形式的伪引用、context 载荷里的 prose、draft locator、坏 id 一律不构成引用；解析全程 total，无 panic 路径；去重上限 64。
  - `collect_checkpoint_recovery_roots` 收窄为**存储保护集**（context 恢复根），先 decode 再读，签名去掉 runs；读授权集（lineage admission）改为 `prepare_restore` 时从已解码 checkpoint 取出、随 `PendingRestore.protected_runs` 传递——**存储保护根与读权限集是不同集合、来自不同证据**（无关保留 checkpoint 的引用不再进入恢复 run 的 lineage）。
  - **降级事实可再取**：`RuntimeStatusSnapshot.restore_evidence_degraded`（有界 64）；`WorkSnapshotResponse.restore_evidence_degraded`（serde default，双侧校验封顶）；两处 `RestoreEvidenceDegraded` 发射点同步写入 actor 状态，下一次 restore 重置。
  - 共享 fixture：`snapshot_response.json` 增空数组字段（紧凑序列化保持字节一致）、`model_used_event*.json` 补 `"role":"main"`（并行线字段收敛）。.NET `WorkSnapshotResponse.RestoreEvidenceDegraded` 镜像＋`FixtureConformanceTests` 回归（空 fixture、超界拒、有界过）。
- **回归**：typed 提取三测（sealed 引用保护/伪引用与畸形 Unicode 不保护不 panic/上界 64）。

## EXEC-7（R2-08/P1）：边界等待移出 Actor 命令分支

**用户结果：** 模型已结束后，GC/checkpoint 维护慢时取消/停止/状态仍有可解释且有界的结果。

- **机制**：`OpKind::Gc` ＋ `state.gc_work`（`PendingGc{operation_id, continuation, task}`）。边界任务经 `OperationCompletion{gc: Some(GcOutcome)}` 回泵，派发按 `gc_work` 所有权 fence（不按 turn stale）。`RuntimeServices` 保留事件 journal 访问；`spawn_runtime` 存 `op_tx` 入 actor 状态，深层路径（safe point、终局）无需贯穿签名。直构 actor（无完成环）回退**内联执行**——与改造前语义逐字节一致（actor 内部测试仍全绿的关键）。
- **落道的边界**：
  1. **turn-final full GC**（`finalize_after_model` 拆为 park＋`finish_turn_final_gc`）：turn-scoped，取消＝abort＋5 秒 join，确认即干净取消（可逆 pass 从未提交）；未确认 → RecoveryRequired 围栏。排队输入 drain 移入真实提交尾（`finish_turn_final_gc`），且终局事务停靠时推迟到其 resume 之后（避免对未提交任务开新轮）。
  2. **终局冻结的 checkpoint 维护**（`commit_completion` 拆为 `commit_terminal_transaction`＋`begin_terminal_freeze`＋`finish_terminal_freeze`）：整笔终局事务（txn、prepared context、prospective 平面、sequence/debt 记账、reply）停靠；停靠期间 `ensure_idle` 拒绝新变异（完成事务不得叠在已移动的任务表上）。resume 复原 freeze 全部错误路径（debt/required_sequence 归还、`fail_terminal_commit` 回滚 prepared＋回执＋ModelProposal 的 `record_completion_commit_failure`），成功路径跑 PHASE Q 尾＋边界 spawn＋回执。停机 drain 有界，超时 abort 并如实说明「已落盘删除仍是事实」。
  3. **完成边界**（full GC＋保留根枚举＋storage GC 合并为一个 spawned op；`compact_after_completion`/`run_storage_gc_at_boundary` 移除，根枚举抽为可 spawn 的 `collect_checkpoint_recovery_roots_for(services)`）：post-commit，结果只变成事件；物理删除不伪称回滚。
  4. **只读 capture**（Checkpoint 命令）：reply 停靠在维护 op 后。
  5. **显式 collect**（同日补遗，本轮收口）：`execute_directive` 的 Collect 分支改为置 `turn.deferred_context_collect` 标记；全轮唯一的下一模型轮漏斗 `spawn_model_operation` 用该标记交换出 Gc op（`GcContinuation::ExplicitCollect`）——pass 落地、`ContextGc` 事件发布后，被推迟的模型决策轮才装配。**collect 仍在下一模型决策前生效**（语义承诺保持），而 actor 命令分支在引擎执行期间保持空闲；预算停止路径（不再有下一轮）在该处内联执行最后一次 collect（保持「现在就收集」）。取消=可逆 pass 干净中止＋回合取消。行为回归（scopes.rs `explicit_collect_stall_keeps_the_command_branch_free_and_defers_the_round`，门控引擎，先红后绿：内联时 status 超时，停靠后 status 应答、`ContextGc`＋TurnCompleted 在放行后到达）；既有 `actor_routes_collect_directive_into_a_full_gc_pass`（两遍 GC 计数）保持绿。
  6. **safe-point 写入**（同日补遗，本轮收口）：Checkpoint 触发的维护改为 spawned **prepare 任务**（只跑引擎调用），冻结的 debt 与已分配 sequence 停靠在 `checkpoint_prepare`；`await_pending_checkpoint` 屏障与 settled-batch 泵负责落地（应用维护报告 → 无维护装配 → 校验 → 序列化 → 写入），失败路径把 debt 原样归还——同步耐久协议的可观察状态不变，而回合提交路径不再阻塞在引擎上。回合末屏障（resume-before-TurnCompleted 顺序）在 prepare 停靠时改为「中继＋停靠回合提交尾」（`GcContinuation::SafepointCommit`）：中继把报告经完成通道送回，resume 先落地 prepare（写盘、`CheckpointDurable`）再跑提交尾，JSONL 顺序保持。取消该停靠尾=回合取消（中继分离不中止，报告仍经屏障落地）。预算停止路径补落地屏障（预算停止欠一个**持久**可恢复快照后才交还控制权），m16 预算停止测试相应改为等待 CheckpointDurable（消除写启动竞态）。行为回归：safe-point 维护停顿期间 status 可应答、放行后 CheckpointDurable 到达。
- **有意不落道的边界（如实记录）**：①**显式 collect** 指令保持内联——它运行在工具批次提交中点，停靠需要把批次续体一并停靠，是另一片的工程量；②**safe-point 写入**保持内联——它是同步耐久协议（debt 冻结＋在飞写入＋ack 退休），屏障调用方在返回即刻观察该状态，改异步需重设计 safe-point 协议。二者在 `execute_directive`/`schedule_checkpoint_write` 注释中写明归属。
- **行为回归**（门控引擎，`tests/turn/safepoint.rs` 新增）：①GC 停顿期间 `status_snapshot` 2 秒内应答、`cancel_turn` 5 秒内返回 `Cancelled`（可逆 pass 干净取消），放行后绝不出现 TurnCompleted；②终局维护停顿期间 status 应答、放行后 commit 回执到达、checkpoint 恰好一条记录。
- **附带修复**：停机 drain 改为「清空所有边界工作」（终局结算会尾随派生完成边界 op，否则 shutdown 提前返回，测试暴露 journal 锁窗口）；`continue_after_gc_work`/`finish_checkpoint_maintain`/`commit_completion` Box::pin 打断 async 递归环。

## EXEC-8（R2-09/P2）：热窗外完成任务的有界冷查询

**用户结果：** 旧任务不在热窗口时，用户仍能查看其结果与产物，或明确知道有界未知。

- **journal 回读**：`EventJournal::read_tail(run_id, max) -> AgentResult<Option<(Vec<envelope>, bool)>>`（provided 默认 `Ok(None)`=后端不支持）；`FileEventJournal` 经 writer 任务串行化实现（`JournalCommand::ReadTail`）：逐行流式＋max 环形缓冲（内存有界与文件大小无关），缺失 trace＝空完整窗口，坏行 typed Err 封死（绝不静默缩短历史）。返回 newest-max 行 oldest-first＋窗口完整性。
- **TaskCompleted 事件**增 `artifacts`（≤33 条，默认空）与 `final_output_digest`（默认 None）——终端提交把记录的有界证据事实写进持久事件，旧事件 serde default 兼容。
- **查询**：`RuntimeCommand::TaskCompletionLookup`＋`RuntimeHandle::task_completion`＋`work::TaskCompletionLookup{Hot, Retired{summary,anchor_revision,artifacts,final_output_digest}, BeyondJournalWindow, Unknown}`。actor 只读 handler：热表命中→Hot；否则扫当前 run＋有界祖先 run 集（`state.journal_runs`，finalize_restore 登记，cap 64），每分区一个 4096 行窗口；找到→Retired（新者胜）；未找到且全窗口完整→Unknown；窗口不完整→BeyondJournalWindow；坏行→typed Err（fail closed，绝不冒充「未发生」）。零模型/零工具副作用。
- **平台/SDK**：协议 `work/task_completion` 路由（`WorkTaskCompletionRequest/Response`＋`WorkCompletionFact` tagged 枚举＋双侧校验：summary≤2000、artifacts≤33×256、digest≤128）；runtime plane handler（复用 `ReadTaskDetail` 授权类）＋宿主 `run_route!` 派发＋共享 fixture `task_completion_response.json`（Rust 字节一致 roundtrip＋.NET `FixtureConformanceTests.Task_completion_fixture_pins_the_retired_fact_cross_language`：多态 fact 解码、超界拒）。.NET `IAgentConnection/AgentConnection/ResumableSession.TaskCompletionAsync`＋Desktop fixture 连接（无 journal → BeyondJournalWindow，诚实）与测试 stub。
- **回归**：storage read_tail 两测（环形＋完整性标志／坏行封死）；全链测试增冷查询断言（ids[0]→Retired{done 0}、ids[65]→Hot、随机 id→Unknown、**冷恢复后同结果仍 Retired**——跨 run 分区经祖先 run 扫描可达）。

## EXEC-4 残余（R2-12/P2）：启动入口同一有界读取契约

- `decode_checkpoint_file` 改为「一柄 take(cap+1)」：超界在读取阶段点名 `exceeds the checkpoint artifact bound` 拒绝（旧代码整文件缓冲后才报无关 parse 错——红实测）；恰 cap 文件过读取门、进入内容校验（错误与大小无关）。句柄读取使 stat 后增长无关紧要。启动 `--restore`/latest 与交互 `/restore` 共用此 helper；`agent-tui` 增 `startup_restore_refuses_an_oversized_file_at_the_shared_bound`（真实启动 wrapper）。

## 验证汇总（本地 Windows，实际执行）

- `cargo test -p agent-runtime`：lib 407、actor 86、instance 4、host 32、shutdown 31、host_restore 3、turn 137 全绿（含 EXEC-5 全链、EXEC-7 两行为测试、EXEC-8 冷查询）。
- `cargo test -p agent-storage`：24 全绿（＋read_tail 2）；`-p agent-platform-protocol`：47＋16（＋跨语言 fixture）；`-p agent-host`：lib 8＋e2e 8＋restore 3＋config 3；`-p agent-tui`：58；`-p agent-compose`（m16_restore、core3_restore_snapshot_paging、kv_cache_walk）：全绿。
- `dotnet test clients/dotnet/Agent.Client.Tests`：**121/121**（＋EXEC-6/8 两个 fixture 回归）；`dotnet build apps/Agent.Desktop`：0 错误。
- `cargo fmt`（B 线五 crate）；`cargo clippy` B 线 crate 0 警告（context-simple/context-baselines 的告警属并行 A 线文件，未触碰）。
- 同日补遗后复验：agent-runtime 全套再绿（turn 139→**140**，含显式 collect 停靠回归），agent-host 22、agent-tui 58＋2、compose（m16 2/core3 1/kv 5）全绿，dotnet 121/121。共享树并行线（COST-7 的 `OperationOutcome::Failed.usage`、provider-openai 重试 usage 携带）编辑中间态由 B 线机械收敛。

## 未验收与限制（如实记录）

- 全部改动未提交、未推送、未跑远端 CI。
- 恢复事件（`RestoreEvidenceDegraded`/快照降级事实）的模型侧可读呈现未做（快照/SDK 可取）；EXEC-7 所有已定位停顿点（turn-final GC、终局冻结维护、safe-point 维护、只读 capture、显式 collect）均已移出命令分支。
- 同日补遗（safe-point 写入维护移道，见上第 5 条）后的复验记录：agent-runtime 全套再绿（turn 139）、agent-host 22、agent-tui 58＋2、compose（m16 2/core3 1/kv 5）全绿、dotnet 121/121；共享树并行线（COST-7 的 `OperationOutcome::Failed.usage`、provider-openai 重试 usage 携带）编辑中间态由 B 线机械收敛。
- EXEC-8：GUI 审阅入口接线归 C 线（runtime/平台/SDK 面已就绪）；journal 窗口 4096 行外的更早任务如实回答 BeyondJournalWindow；真实 provider 照旧 NOT_RUN。
- 本轮未调用付费模型；共享树并行线的编辑中间态（contracts 字段演进等）由 B 线机械收敛，未覆盖其语义决策。
