# A3 落地回执（续）：保序命令提交与慢 I/O 离开绘制循环（U6）

- 基线：`d92564bcfa41dda44f752e1e49e88a321abdf942`
- 提交：`96c0e5c3`（`tui: U6 ordered command lane, identity-checked actions, off-loop I/O`）
- 范围：`crates/agent-tui/src/session.rs`、`state.rs`
- 依据：[REVIEW.md](REVIEW.md) U6、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) A3
- 与 U5 的关系：A3 的另一半 U5 见 [A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT.md](A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT.md)。**本片完成后 A3 全部关闭。**

## 用户动作

命令按用户键入顺序生效；操作员观察到的任务就是命令作用的任务；慢速保存/恢复期间仍能输入、取消、退出。

## 修复前 / 修复后

| 问题 | 修复后 |
|---|---|
| `/focus`、`/task`、`/done`、`/continue` 各自 detached spawn → Actor 只保证「到达之后」的顺序，`/task B` 紧接 `/continue` 可能反序到达并继续旧任务 | 单一**有界**队列（`COMMAND_QUEUE_CAP = 32`）＋**唯一 worker** 按提交顺序消费 → 键入顺序即送达顺序 |
| 队列满/worker 消失时命令静默丢失 | `submit_command` 显式报告：「command queue is full (32); … was NOT submitted」/「command worker is gone」 |
| `/continue` 用无期望身份的兼容方法 | `continue_active_task_expecting(observed)`；不匹配**不启动任何回合**，通知同时列出「你观察到的任务」与「实际活动任务」 |
| `/suspend` 用无期望身份版本 | `suspend_task_expecting(observed)`，不匹配则拒绝并说明 |
| `/done` 无身份校验 | 有序提交 ＋ worker 内 `status_snapshot()` 前置比对，不匹配**拒绝关闭** |
| `/checkpoint`、`/restore`（含磁盘读）在输入循环内 `await`，慢盘冻结键盘与帧 | 两者进入同一 worker：worker 持有 `RuntimeInstance::checkpoint_plane()`（公开的 `Arc<RuntimeCheckpointPlane>`），运行时捕获与原子存储写都**离开绘制线程**；保存的 artifact 经类型化 `ViewFact` 回传，循环应用后 `/status` 仍显示它 |
| 每帧无界 drain 事件 | `DRAIN_BUDGET_PER_FRAME = 2048`，事件洪泛不再饿死键盘 |
| `Lagged` 提示使用自家旧措辞 | 复用 U3 的 `view_partial` 原因，与 `/status` 一致 |

**边界（刻意保留）**：`/quit` 与 `/cancel` **不排队**，留在循环上——逃生通道不能排在慢操作后面。

`observed` 取值：`AppState::observed_task`，由 `FocusChanged`／`TaskCompleted` 设置，`FocusCleared` **不清除**（操作员最后一次观察到的仍是那个任务）。

## 回归

新增 4 条测试：

| 测试 | 断言要点 |
|---|---|
| `command_queue_tests::queued_commands_are_delivered_in_submission_order` | 单消费者按提交顺序收到命令 |
| `command_queue_tests::a_full_queue_reports_the_drop_instead_of_losing_the_command` | 满队时报告丢弃，不静默吞掉 |
| `command_queue_tests::a_closed_worker_is_reported` | worker 消失被报告 |
| `command_queue_tests::an_identity_mismatch_names_both_tasks` | 拒绝文案同时点名两侧（含 observed=None 场景） |

**既有 e2e 覆盖了新链路的端到端路径**：`tui_e2e_budget_checkpoint_restore_continue_lands_the_next_segment` 走的是 预算停止 → `/checkpoint` → `/restore` → `/continue`，这三条命令现在**全部经过新队列与 off-loop worker**，该用例保持绿色。

**变异恢复法复验**（sha256 校验备份，改完立即还原）：

| 变异 | 结果 |
|---|---|
| 满队分支改为静默丢弃 | 满队报告用例 **FAILED** |
| 拒绝文案丢掉 live 任务 | 身份不匹配用例 **FAILED** |

## 已执行验证

```
cargo test -p agent-tui          → 87 passed; 0 failed + real_binary_startup 2/0
cargo fmt -p agent-tui -- --check → clean
cargo clippy -p agent-tui --all-targets -- -D warnings → 0
```

## 限制（如实）

- **`/done` 不是原子保证**：`RuntimeCommand::CompleteTask` 在共享契约里没有 `expected_task_id`。本片用「worker 内紧邻调用前的 `status_snapshot()` 比对」缩小窗口，并在不匹配时拒绝；**真正原子需要给共享接口加 expecting 变体**，属于跨 crate 契约变更，未在本片做（审查也要求不在 TUI 自行完成任务）。
- **身份不匹配路径没有 e2e 回归**：只有文案级单测。构造「观察 A 但运行时在 B」的真实 actor 场景需要更重的 harness，本轮未做。该保证部分依赖 `agent-runtime` 自身的 `*_expecting` 语义与其测试。
- `observed_task` 由事件派生：若 UI 丢事件且 `/status` 显示 PARTIAL，观察值本身可能不是最新（此时 U3 的 PARTIAL 提示会同时出现）。
- 队列顺序只覆盖进入队列的命令（任务动作 + checkpoint/restore）。只读命令（`/tasks`、`/grants`、`/review`、`/plan`、`/context`、`/pin` 等）仍是各自 spawn，**它们之间及其与队列命令之间不保证顺序**——本轮按审查范围只收口「作用于任务身份」与「慢 I/O」两类。
- 慢 I/O 离开的是**绘制循环**，不是**运行时**：`checkpoint_plane()` 与其它命令仍由 RuntimeActor 串行执行；本片没有、也不应该新建调度器。
- 未跑 workspace 全量 CI；未做真实 PTY 下的键盘响应计时验证（`DRAIN_BUDGET_PER_FRAME` 只有界，未做延迟测量）。
