# R 批次第一批回执：流式身份、费用结算、卡片发布与失败计数（R1／R2／R3／R4／R5）

- 基线：`3bdb269cba4e1bdd0fe2e71007dfa1ad7640b28d`
- 提交：
  - R1 `bb761547`（`tui: R1 stop deduplicating live stream fragments by journal cursor`）
  - R2＋R3 `8877a4da`（`runtime: R2 settle usage from every late terminal shape; R3 usage never clears activity`）
  - R4＋R5 `d30f2956`（`tui: R4 publish order is the session's, not the card's; R5 count failures before the cap`）
- 依据：[REVIEW.md](REVIEW.md) R1–R5、[NEXT_ACTIONS.md](NEXT_ACTIONS.md)
- **R6／R7 的其余部分／R8 未在本批完成**，见文末。

## R1：日志游标不是分片身份（P1，已确认的真实回归）

**已核实的生产者形状**：`sink.rs::LiveSink::new(core.event_sender(), core.event_sequence(), …)` 把 `ModelStarted` 的持久游标存为 `journal_cursor`，`emit_live()` 用它作为每个 `ModelDelta`／`ModelRetrying` 的 `seq`——分片不写 WAL、不申请新序号。U3 的 `claim_event(RunId, seq)` 因此让首个事件占住键，后续分片在最外层直接返回：无流式文本、无重试进度，且 `current_op` 代际围栏根本没跑到。

**修复**：**契约里本来就有** `RuntimeEvent::is_live_only()`，其文档正是"分片复用前一条持久游标……投递游标不得过滤它们"，且 `agent-host` 已在用（`envelope.seq > watermark || envelope.event.is_live_only()`）。TUI 只是没有采用。现在 `apply_runtime_event` 让 live-only 事件**同时跳过持久 claim 与重放水位**，归属交给它们真正携带的身份 `(TurnId, OperationId, generation)`（既有 `current_op` 围栏）。

**为什么没有测试挡住（结构性原因）**：折叠用例的 `envelope()` helper 每次调用都新建 `RunId`，却把 `seq` 固定为 1——**恰好绕过了消费者实际应用的那套身份**。已修正为「一个稳定 RunId ＋ 递增日志序号」。同时 `status_projection_tracks_task_debts_and_checkpoint` 原本复用 `seq 2` 与 `3`（真实日志不会重复序号），已改为严格递增；**没有削弱任何断言**。

**新增回归**：`live_fragments_reusing_the_start_cursor_still_render`、`a_live_fragment_from_a_superseded_generation_is_still_dropped`。

## R2：迟到终态的用量结算（P2）

修复前 stale 分支的三臂结构为：`已 accounted` → 仅当 outcome 是 `Cancelled{known_usage: Some}` 才补账；`else if ModelOutput{usage}` → 记账；`else if Maintenance` → 记账。因此**取消屏障写下 Unknown 占位并把 operation 标记 accounted 之后，迟到的 `ModelOutput{usage}`／`Failed{usage}` 落不进任何一臂**——业务结果被正确作废，已知费用一起被丢弃。

**修复**：新增 `outcome_reported_usage(outcome)`，把用量提取从业务结果分支中独立出来，覆盖 `ModelOutput`／`Failed`／`Cancelled`；并把原先被混为一谈的两个事实分开：

| 事实 | 标记 | 含义 |
|---|---|---|
| 写了 Unknown 占位 | `accounted`，**未** settled | 仍允许**恰好一次**迟到证据补账（W4 原形状保留） |
| 已知值已入账 | `accounted` ＋ settled | 不可再加证据 |

`mark_usage_supplemented`／`usage_supplemented` 更名为 `mark_usage_settled`／`usage_settled` 以反映新语义；**复用既有队列字段**，因此没有触碰并行会话正在编辑的 `actor/mod.rs` 状态声明。

**新增回归**（`crates/agent-compose/tests/cancel_late_result_usage.rs`）：provider 调用**先挂起**，等操作员取消被处理后再放行——即审查所说的合法竞态（供应商**不需要**忽略取消）；两种终态分别断言「业务保持取消、无工具执行、真实计数入账恰好一次」。既有 W4 回归 `cancel_during_backoff_keeps_known_usage_and_cancelled_state` 保持通过。

## R3：用量事实不得清掉当前操作状态（P2）

`StatusProjection::fold(ModelUsed)` 原有无条件 `in_flight = None`。Runtime 合法地会把**已失效 operation 的迟到用量**送进当前事件流，于是「A 取消 → B 运行 → A 的迟到用量」会把仍在运行的 B 显示成"没有正在执行的操作"。

**修复**：用量行只推进**账目**；活动状态由能命名自己终止对象的生命周期事件推进（`ModelStarted`／`ToolStarted`／`ToolFinished`／`TurnCompleted`／`TurnCancelled`／`RuntimeRestored`）。没有丢弃任何迟到用量。

**新增回归**：`a_late_usage_row_does_not_clear_the_current_operation`。

## R4：发布序是会话的，不是卡片的（P2）

`begin_card_for_task` 让新卡 `revision` 从 0 起，而 `card_snapshot_gate.last_written_revision` 是跨任务共用的全局水位 → A 完成后水位为 1，B 的卡 revision 又是 1，`1 <= 1` 被拒 → **B 的卡内存正确、磁盘仍是 A**。

**修复**：引入会话单调的 `card_publish_seq`（切任务与投影重建都不重置），写入器按它排序，artifact 增 `publish_seq` 以便跨任务排序；卡片自身 `revision` 仍是"本任务卡的内容修订号"。单写者与 rename 原子提交保留。

**顺带（R7 的一部分）**：重放期间设 `replaying`，历史 `TaskCompleted` **不再**逐个触发快照写入——重放是读过去，不是新投递。

**新增回归**：`consecutive_tasks_all_publish_and_the_newest_wins`（同一 AppState、真实临时目录、连续完成 A/B/C，读回磁盘为 C）。

## R5：省略的失败必须进入失败总数（P2）

`failed_checks()` 只统计仍在 32 行显示窗口内的失败，却与 `total_checks()` 并列显示 → 「32 通过 ＋ 第 33 个失败」显示为 `33 recorded, 0 FAILED, 1 not shown`。

**修复**：失败在**事件处、容量裁剪之前**计数（`failed_checks_total`），显示行把**账目**与**窗口**分开表述："N recorded, M FAILED (of which K in the 32-row display window), J not shown"。事件处计数对重投递安全，因为 `apply_event` 每个持久事件最多执行一次。原窗口内计数访问器更名为 `failed_checks_in_window`，避免被误当作账目。

**新增回归**：`an_omitted_failed_check_does_not_read_as_zero_failures`。

## 变异恢复法复验（sha256 校验备份，改完立即还原）

| 被还原的修复前行为 | 转红用例 | 观察到的症状 |
|---|---|---|
| 去掉 `is_live_only()` 门控 | `live_fragments_reusing_the_start_cursor_still_render` | 助手正文为空 `""` |
| 用量抽取只认 `Cancelled` | 两条 `cancel_late_result_usage` | 账目只剩 `[(0, 0, 0, Unknown)]`，真实计数丢失 |
| `ModelUsed` 重新清 `in_flight` | `a_late_usage_row_does_not_clear_the_current_operation` | `in_flight=none` 而新轮次仍在运行 |
| 用卡片 revision 当发布水位 | `consecutive_tasks_all_publish_and_the_newest_wins` | 最新任务卡写不到盘 |
| 用窗口计数当账目 | `an_omitted_failed_check_does_not_read_as_zero_failures` | 失败被报成 0 |

## 已执行验证

```
cargo test -p agent-tui                      → 91 passed; 0 failed + real_binary_startup 2/0
cargo test -p agent-runtime --lib            → 424 passed; 0 failed
cargo test -p agent-compose --test cancel_usage_settlement   → 1 passed（W4 未退化）
cargo test -p agent-compose --test cancel_late_result_usage  → 2 passed
cargo fmt -p agent-tui -- --check            → clean
cargo clippy -p agent-tui --all-targets -- -D warnings → 0
```

## 未完成与限制（如实）

- **R6 未做**：`session.rs` 里普通非 `/` 文本仍走独立 `tokio::spawn(handle.user_message(…))`，**没有进入保序通道**；慢 checkpoint 期间 `/task B` 之后的普通纠正文本仍可能先到达旧任务。worker 的 `JoinHandle` 仍未由 session 持有，退出时未定义未执行队列的结算。
- **R7 只做了发布副作用与（R4 一并的）重放不发布**：**Warning／FocusChanged／ModelUsed 等产生的 SYSTEM 行仍缺少事件身份去重**，重放仍可能重复追加并把助手行挤出窗口；`shown_input_ids` 仍是只插不淘汰的 HashSet（无界残余）。
- **R8 未做**：`wrapped_rows` 仍是 `display_width ÷ 宽度` 估算（与 Ratatui 按词边界折行不等价），手写 Unicode 宽字符表对组合附加符／ZWJ 不等价于 `UnicodeWidthStr`。**未执行**任何 TestBackend 窄窗/组合字符渲染反例。
- **R1 的证明是 agent-tui 级的固定 RunId 分片形状**，不是通过真实 `LiveSink` 端到端驱动；`sink.rs` 侧未加"仅测试暴露"（未改动该文件）。真实 PTY 下的流式显示仍未验证。
- `ModelUsed` 不再清 `in_flight` 后，**模型轮结束后到下一个生命周期事件之间**，`/status` 会继续显示 `in_flight=model round` 而不是 `none`。这是"无法证明归属就不改活动状态"的代价，属**有意取舍**，不是遗漏。
- `maintenance` 分支的"未知行回退"（`Failed` 无 usage）仍按占位处理（未 settled），允许一次迟到证据补账；若未来同一 operation 存在**多份增量证据**，需要明确累计值/增量值语义而非直接相加（审查已提示，未实现）。
- **B1／B2 未动**：`context-simple` 仍有并行会话的未提交改动。
- 未跑 workspace 全量 CI；未推送。
