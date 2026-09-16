# R 批次第二批回执：命令顺序与渲染口径、重放幂等、导出取消安全（R6／R7／R8／B3）

- 基线：`3bdb269c`（R6/R7/R8）；B 线在飞工作已先按归属提交（`ca6c7254`、`99218532`）
- 提交：
  - R6＋R8 `a0f23d72`（`tui: R6 ordinary input joins the ordered lane; R8 reuse the widget's wrapping and unicode-width`）
  - R7 `ef8a835c` ＋ 修复 `1a3ec323`（`tui: fix a clippy failure in the R7 regression (useless vec!)`）
  - B3 `3fe396c9`（`context-simple: B3 ledger export commits first, consumes after`）
- 依据：[REVIEW.md](REVIEW.md) R6／R7／R8、[NEXT_ACTIONS.md](NEXT_ACTIONS.md)；B3 沿原工单编号

## 在飞工作的归属（先说清）

工作树里原有 374 行未提交改动（`context-simple` 的 foreground 未读区分 ＋ `agent-runtime` 的 `ResourceFactKind`）。用户在本次会话中确认**没有其他 agent 在执行**，因此在改动它们之前先把这份工作**按归属分成两个提交**（`ca6c7254`、`99218532`），并在提交信息里写明它是**先前存在**的在飞工作、已验证绿（agent-runtime lib 424/0、context-simple 429/0、fmt clean）。这样 B 线后续提交才能建立在干净基线上，且 374 行已验证的工作不再无保护地悬着。

## R6：普通输入进入保序通道

普通非 `/` 文本原先走独立 `tokio::spawn(handle.user_message(…))`。worker 在慢 checkpoint 上停顿时，`/task B` 排队而其后输入的纠正文本**直接到达上一个任务**——Actor 只能串行处理"已经到达"的命令，无法恢复键入顺序。

- 新增 `SessionCommand::Input`，普通文本走**同一条有界通道**。
- 新增 `CommandLane`（通道 ＋ 未开始执行的提交计数）；`run_session` **持有 worker 的 `JoinHandle`**，退出时停止接单、取消未开始项并**报出条数**（stderr ＋ 转录，转录那份是 best-effort——终端紧接着就被恢复，这一点如实写明而非假装）。
- `/quit`、`/cancel` 仍不排队（逃生通道不能排在慢 I/O 后）。

**测试**：`ordinary_text_cannot_overtake_a_pending_task_command`（worker 未消费时两条提交仍按序取出）、`the_lane_reports_submissions_that_have_not_started`；既有 `tui_e2e_*` 全套（含发送普通文本的用例）保持通过，证明路由端到端可用。
**未覆盖**：没有构造"暂停 worker"的真实 actor 端到端反例，因此**路由改动本身没有直接的红用例**——顺序断言在 lane 层，路由由 e2e 覆盖。

## R7：重放重建事件派生视图，不再追加

每个事件派生的行现在带**产生它的事件身份**；resync 先丢弃这些行（连同守护它们的身份集合）再重放日志。会话本地行（开场横幅、命令提示、离环回复）没有身份因而存活——这正是既有"SYSTEM 横幅必须保留"断言的语义。

修复前：User/Assistant/Tool 行有去重，但 `Warning`／`FocusChanged`／`ModelUsed` 等产生的 SYSTEM 行**没有身份规则**，重放会再次追加；在填满的 400 行窗口里这些重复行把真实对话行挤出去。**持久日志完好，可见转录不稳定**——同一份日志读两次得到两个屏幕。

`shown_input_ids` 原是无淘汰的裸 `HashSet`；现已改为有界 FIFO＋索引，并随它保护的行一起清理。**这收口了 R7 的全部内容。**

**测试**：`replaying_the_same_journal_twice_yields_the_same_transcript`（同一日志连续重放两次，可见转录逐行相等）。

## R8：复用库的折行与宽度语义

- 滚动上限原先用 `display_width(line).div_ceil(width)` 估算，而 ratatui 按**词边界**折行、可能要更多行。10 列窗里 12 行「三个 6 字符词」实际需要 36 行而除法只给 24 → 上限短 12 行 → **尾部永远不可达**。会话与审批两处上限现在都问控件本身（`Paragraph::line_count`，经 ratatui 的 `unstable-rendered-line-info` 特性启用），**上限与绘制使用同一条折行规则**。
- 手写 Unicode 宽字符表删除，`display_width` 委托 `unicode-width`（渲染布局所用的同一个 crate）。该表把组合附加符与 ZWJ 各算 1 列，所以 `e`＋U+0301 会被算成 2 列。

**测试**：`a_narrow_pane_still_reaches_the_approval_tail_when_words_wrap`（真实 `ui::render`＋`TestBackend`，12×24 窄窗翻到底后尾部可见）、`a_combining_mark_does_not_advance_the_input_cursor`（组合字符不额外推进光标）。

**依赖变更**：工作区 `ratatui` 增 `unstable-rendered-line-info` 特性，`agent-tui` 增 `unicode-width.workspace` 直接依赖。**注意**：该特性在 ratatui 侧标注为**不稳定**（其文档明说文本折行设计尚不稳定），是审查明确建议的评估项之一；若上游改变该 API，`wrapped_line_count` 是唯一适配点。

## B3：导出先提交、后消费

原实现 `mem::take` 走 ledger 再 await 写入，**只在 I/O 错误分支**回灌。I/O 失败因此不丢行——但**被取消的导出**（future 在 await 中被丢弃）根本不会进入那个分支，行随局部变量消失。既有回归只覆盖错误路径，这正是它一直未闭合的原因。

现在：快照（不取走）→ 写临时文件 → rename 提交 → **确认消费该 artifact 所承载的行**（`ledger::confirm_exported`）。逐一前缀比对：行只在尾部追加、只在头部淘汰，因此除非有界容量在写入期间淘汰了部分行，导出的行仍是头部前缀；一旦不匹配就**什么都不消费**——宁可重复也不删除从未导出的行。

`ContextLifecycleRecord` 增 `PartialEq/Eq`（对契约是加法）。

**测试**：`a_cancelled_ledger_export_consumes_nothing`（只 poll 一次导出 future 后丢弃——正是审查点名的 write/rename 边界——断言行数不变且随后仍可导出）；既有 `failed_ledger_export_merges_rows_back` 保持通过，错误路径未变。
**范围**：这是 Context 生命周期 ledger 的引擎内导出缓冲，**不是** Core authority WAL，也不是任务正文丢失。

## 变异恢复法复验（sha256 校验备份，改完立即还原）

| 被还原的修复前行为 | 转红用例 |
|---|---|
| 宽度除法当折行行数 | `a_narrow_pane_still_reaches_the_approval_tail_when_words_wrap` |
| 重放不丢弃、继续追加 | `replaying_the_same_journal_twice_yields_the_same_transcript` |
| await 前 `mem::take` | `a_cancelled_ledger_export_consumes_nothing` |

## 已执行验证

```
cargo test -p agent-tui                              → 96 passed; 0 failed + real_binary_startup 2/0
cargo test -p context-simple                         → 430 passed; 0 failed（含 B3 新用例）
cargo fmt -p agent-tui -- --check                    → clean
cargo clippy -p agent-tui --all-targets -- -D warnings → 0（用退出码校验，不再被管道掩盖）
```

## 未完成与限制（如实）

- **B1 未做**：批量 required 冷解析仍未产出有界、版本/范围绑定的 `RequiredPlanSource`；解析结果仍只记 `Installed/AlreadyOwned`，规划仍依赖整批结束时哪些目标恰好还在热目录。**这是审查列出的最大剩余项。**
- **B2 未做**：`run_external_spill_io` 遇到已有卡片路径仍仅凭 `try_exists` 就放入 `io.written/io.spilled`，未校验现有 bytes 与计划内容/hash/身份一致；同名坏文件条件下新 checkpoint 仍可能只引用坏卡片。
- **R6 的路由改动没有直接红用例**（顺序断言在 lane 层，路由靠 e2e 覆盖）；没有构造暂停 worker 的端到端反例。
- **退出时未执行命令的计数只在转录里 guaranteed 可见性不足**：终端紧接着恢复，`eprintln!` 会被 alternate screen 抹掉。
- **`unstable-rendered-line-info` 是上游不稳定性 API**；上游若改动，`wrapped_line_count` 需要适配。
- 未跑 workspace 全量 CI（`cargo test --workspace` / `clippy --workspace`）；未推送。
