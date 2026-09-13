# EXEC-2 实施回执：完成任务的热投影有界（M18 B 线第二片）

基线：`685b6bbb` ＋ 既有未提交工作树（多线共享树）。对应 [REPORT.md](REPORT.md) **E06（P2）**、[TASKS.md](TASKS.md) EXEC-2。工作树落地（未提交、未推送、未跑远端 CI）。

## 用户能获得什么

同一宿主长期使用、完成成百上千个任务后：任务查询与 checkpoint 持续可用（快照行数与字节停在已说明的硬预算内，不再逼近 16 MiB payload 上限），旧结果仍可经持久事实（run journal 的完成事件、sealed final-output 工件及其 digest）审阅。活跃/挂起任务与完成权威永不因淘汰消失。

## E06 复盘与本片修复

**缺陷**：`TaskManager` 的 `tasks`/`completed` 两个 Vec 没有任何移出路径——`MAX_TASK_RECORDS=256` 只数非 Completed 行；完成的任务行（连同完整 anchor/resume）永久留在热表，完成记录无限追加；`TaskManagerSnapshot::from_manager` 与 `prospective_terminal_snapshot` 全量复制，checkpoint 随完成年龄无限增长。

**修复（有界热驻留＋引用式审阅，不建第二权威）**：

- 新常量：`MAX_HOT_COMPLETED_TASK_RECORDS = 64`（完成任务的完整热行窗口）与 `MAX_HOT_COMPLETION_RECORDS = 256`（完成记录窗口）。两者只作用于完成事实；resumable 行（active/suspended）不受影响，256 上限照旧。
- `TaskManager::enforce_hot_bounds`：在每次 Complete 提交后与 `restore` 装入后执行——完成热行按最旧淘汰（`last_active_ms, created_at_ms`），完成记录最旧先出；淘汰计数器（`evicted_completed_task_records`/`evicted_completion_records`）单调累计。恢复 oversized 旧 checkpoint 时套用同一窗口（最新保留、计数如实），恢复后的运行时不能继承无界增长。
- **淘汰不删盘**：完成事实的持久权威是 journal 的完成事件与 sealed final-output 工件（ref＋digest 在完成记录/Journal 中，由 artifact store 自身的保留规则管辖）；本边界不触碰任何磁盘对象。完成权威（一条完成记录对应一次完成）不被丢弃——被淘汰的是**热可查询窗口**，read model 对窗口外的查询如实报告（`completion_of`/`get` 返回 None，不伪造）。
- **诊断暴露**：新 `TaskHotStateSummary`（resumable/hot-completed/completion 计数＋两个淘汰计数器）经 `TaskManager::hot_state_summary()` 进入 `RuntimeStatusSnapshot.task_hot_state`（serde 兼容内部类型）——「任务表是否仍有界」是可读数字，不是观察某个 Vec。
- 背压：完成路径无接纳压力（完成减少热状态）；创建路径既有的 256 resumable fail-closed 保持。

## 回归（task.rs 单元层，4 项新增）

1. `completed_hot_rows_retire_at_the_bounded_window_and_resumables_survive`——`MAX_HOT+10` 次合法完成后：热行恰为 64、淘汰计数 10、早期完成行离开热表、**挂起的 resumable 任务存活**（resumable 不受淘汰影响）。
2. `completion_records_stay_queryable_inside_the_hot_window`——300 次完成后：记录窗口 256、淘汰 44；近期结果可查（summary 逐字），早期结果离开热窗口（事实归 journal）。
3. `the_checkpoint_task_snapshot_stays_bounded_across_a_thousand_completions`——**1000 个任务经合法 create→complete 路径完成**后：快照完成行恰 64、完成记录恰 256、序列化字节 < 2 MiB（对照 16 MiB payload 上限，留足其他段空间）。
4. `an_oversized_legacy_snapshot_restores_inside_the_bounds`——100 完成行＋300 完成记录的 oversized 旧快照 restore 后落在同一窗口内（淘汰 36/44 如实计数），**active 任务保留且可查**。

## 实际执行的检查（本机 Windows，2026-09-12）

- `cargo test -p agent-runtime --lib`：**397/397**（393＋新增 4）。
- `cargo test -p agent-runtime --test actor`：**86/86**；`cargo test -p context-baselines`：18 通过；`cargo test -p agent-compose`：全绿（含 CORE-3 恢复分页走查）。
- `cargo fmt -p agent-runtime`：已应用（含并行线遗留的 effects.rs 格式漂移，纯格式无语义）；`cargo clippy -p agent-runtime --all-targets`：**0 警告**。
- **如实记录的树上并行域失败（非本片引入）**：`turn::safepoint::failed_checkpoint_write_fences_continuation_until_a_retry_lands`（EXEC-1 回执已归因：新旧路径同败）；`context-simple` 的 distill/supersession 测试失败集随 A 线会话实时变动（当前为 CTX-3 distill 来源清单在飞测试，测试名与任务书 CTX-3 回归名单一致）。本片未触碰这两个域。
- 验证期间 C 线（COST-1/4）与 A 线（CTX-2/3）在同一共享树活跃落码；`context-baselines`/`context-simple` 曾处于其编辑中间态，均已在他们落定后恢复编译并通过。

## 边界与下一片

- 未重建 run catalog 数据库、不自动删除用户结果、不重新激活终态、不弱化完成/effect 语义。
- 平台 wire（WorkSnapshotResponse）未新增字段——RuntimeStatusSnapshot 是内部诊断面；SDK/GUI 需要展示热态计数时随 B4「增量字段」模式补 wire 字段。
- **未验收**：未提交、未推送、未跑远端 CI；真实多任务长程旅程照旧留待统一验收。
- **下一片**：EXEC-3（恢复后的活跃 artifact 引用受保护）。
