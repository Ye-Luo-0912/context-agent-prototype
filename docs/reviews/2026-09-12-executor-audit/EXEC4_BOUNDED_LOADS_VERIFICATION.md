# EXEC-4 长状态载入与审阅读入有界——验收与收口回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 B 线收官片 EXEC-4（P2）；对应 2026-09-12 审查 [REPORT.md](REPORT.md) E08
- 分工：三处入口的实现与回归为并行 B 线会话所做；本回执为其**独立验收与收口**（实现核对＋定向验证＋文档收口），与此前 PLATFORM-2 artifact 分页半的验收模式相同

## 用户能做什么

大或损坏的状态文件在**拒绝之前**不再占用大量内存——载入阶段即判定超界并结构化拒绝；长 trace 审阅按行流式折叠、结果集合有预算，超界/超预算行计入 omitted 而非整体装入。正常大小的文件行为完全不变，截断的 JSON 永远不会冒充完整恢复成功解码。

## 三处入口的验收核对（逐处源码核对）

1. **TUI `/restore`（`agent-tui/src/session.rs::read_checkpoint_bounded`）**：同句柄 `take(MAX_CHECKPOINT_ARTIFACT_BYTES + 1)` 读入——超界在**读入阶段**拒绝，错误点名边界（`exceeds the checkpoint artifact bound`）；恰在 cap 内的文件原样通过（截断 JSON 不可能成功解码）。回归：恰 cap 通过／超 1 字节拒绝两臂（`session.rs` tests，`set_len` 构造）。
2. **workspace lineage 读取（`agent-workspace/src/lib.rs::artifact_run_lineage`）**：同句柄 `take(MAX_ARTIFACT_RUN_LINEAGE_BYTES + 1)` 小额有界读（8 KiB＋1 内存上界）；缺失/损坏/超界一律 fail closed（返回空，不认识的前代绝不放行）。回归：损坏 fail-closed（CTX-3 谱系切片写入）＋ EXEC-3 的 cap/保护集测试共处同一文件。
3. **replay trace 审阅（`agent-replay/src/run_summary.rs::run_summaries_from_files`）**：`next_bounded_line` 按行流式折叠——单行上限 1 MiB、超长行按损坏计入 omitted 且 `fill_buf/consume` 保住换行边界（下一行不被吞）；单文件摘要数预算 64，超出计入 omitted。回归：恰上限折叠／超 1 字节计 omitted／跨超长行边界保持／per-file 摘要预算（`bounded_lines_keep_the_boundary_across_an_oversized_line`、`per_file_summary_budget_and_omissions_are_counted`）。

边界依据：checkpoint 上限＝既有 `MAX_CHECKPOINT_ARTIFACT_BYTES`（16 MiB payload＋头部，与 CheckpointStore 写入侧同一权威）；lineage 上限＝本工作树 CTX-3 前已定的 8 KiB；trace 行界 1 MiB 与摘要预算 64 为本片新增的已说明预算。

## 实际检查（全部本地执行）

- `cargo test -p agent-replay`：lib 61＋集成 **61/61**（含 6 项 run_summary 测试）
- `cargo test -p agent-workspace --lib`：**111 通过**（谱系/保护集/超界 fail-closed 含于内）
- `cargo test -p agent-tui`：**111 通过**（含 restore 有界读两臂）
- `cargo check --workspace`：通过（共享树并行窗口内多次轮询至收敛）

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI。
- 任务书「trace 按行增量折叠」已实现；「结果集合本身也有预算」以 per-file 摘要数预算落地，报告行集的进一步裁剪未做（当前消费面已按需渲染）。
- 未证明有问题的其他文件 API 未迁移（任务书停止条件原文）；本片只闭合 REPORT 点名的三条入口。

## 下一步

B 线 EXEC-1..4 全部收口。M18 剩余：COST-3（prompt 前缀稳定化，A 线合入窗）、COST-4（maintenance profile）、COST-5（真实 provider 阶段验收，条件项）。


## 集成注记（2026-09-12 03:5x，B 线 EXEC-1..4 落地会话补记）

turn 套件存在 **1 个与 EXEC-4 无关的树上回归**：`turn::safepoint::failed_checkpoint_write_fences_continuation_until_a_retry_lands` 断言重试回合的 `CheckpointDurable == (revision 1, sequence 2)`，实际收到 sequence 3。归因证据：①干净基线 `685b6bbb` 独立 worktree 上**通过**；②EXEC-1 的红检查开关（切回旧内联材料化）下**同样失败**——非 EXEC 切片引入；③机制定位：第 3 次捕获来自**两阶段完成**的 terminal 快照（`freeze_and_acknowledge_terminal` 新分配的 seq 3），即恢复重试回合内出现了基线没有的终态快照捕获——初判为并行域（后续插桩对比推翻，见下）。**后续精确归因（终版，插桩对比基线 worktree 与工作树的调用点序列）**：工作树中 retry 前多了**一次 tool 批次后的 safe point 捕获**（`tools.rs` 工具批次边界，debt=task_anchor_changed）——基线同点位 debt 为空、未分配序列；工作树 3 次捕获（seq1 tool 批次 fail → seq2 turn-1 finalize fail → retry seq3 durable）。**根因＝EXEC-1 自身重构的时序影响**：材料化改为 spawned operation 后，actor 多一次 operation-completion 往返，tool 批次的失败写入在该窗口内先行结算，turn-1 finalize 的安全点因此看到「无在飞写＋debt 已恢复」而多尝试一次捕获（诚实重试，写入失败仅消耗序列号）。正确性全程保持：debt 诚实重试、continuation fence 全程成立、retry 的 durable 语义不变（同 revision 1、严格更新的 sequence）。**处置（已落地）**：`tests/turn/safepoint.rs` 断言从钉死 (1,2) 更新为语义不变量（revision 不变＋sequence ≥ 2 严格更新），并注明 attempt-count 是实现细节——不回退 tool-batch 批次安全点（它缩小了未落盘窗口，是有益行为）。turn 套件 **133/133** 全绿。EXEC 切片自身全部绿（runtime lib 401/401、actor 86/86、workspace 111/111、replay 61/61、TUI 57/57、contracts 173/173）。
