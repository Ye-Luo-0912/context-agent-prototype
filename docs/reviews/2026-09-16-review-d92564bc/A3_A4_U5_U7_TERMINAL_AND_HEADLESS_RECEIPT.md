# A3／A4 落地回执（部分）：终端恢复 guard（U5）与 headless 完整性（U7）

- 基线：`d92564bcfa41dda44f752e1e49e88a321abdf942`
- 提交：`296ec005d2a458387c84a6b76a4cacc11811ac28`（`tui: U5 terminal restore guard & U7 headless completeness`）
- 范围：`crates/agent-tui/src/main.rs`、`cli.rs`
- 依据：[REVIEW.md](REVIEW.md) U5/U7、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) A3/A4

**本回执只覆盖 U5 与 U7。U6（保序命令提交、慢 I/O 离开绘制循环）未实现**，见文末。

## U5：终端恢复独立于 Runtime shutdown

- 新增 `TerminalGuard`，经 `TermBackend` 抽象驱动 `enable_raw_mode / EnterAlternateScreen / clear`，跟踪**本进程实际开启了哪些**状态，按逆序在早退、`Drop` 与 panic hook 中恢复。
- **早退不再泄漏**：`open` 逐级启用，任一步失败时先把已启用的部分状态恢复，再返回错误。因此 raw mode 开启之后没有任何 `?` 能把终端留在 raw/alternate。
- **panic hook**：只安装一次，恢复终端后**链回原 hook**，不吞掉原有 panic 行为。共享 `ACTIVE_TERMINAL` 状态让 hook 能在拿不到 guard 值时仍恢复。
- **两个责任分开结算**：`real_main` 先 `term.restore()` 释放终端（错误逐条打印并单独聚合），再执行有界的 `composed.shutdown()`；任一路径的失败都各自报告，不互相掩盖。
- 明确**不**宣称能恢复 SIGKILL 之后的终端。

## U7：headless 成功必须有正面终态证据

- `Lagged` 不再只打 warning：记录 `events_dropped` 与 `dropped_count`。接收端在**任何终态之前**关闭则记录 `closed_without_completion`。
- 两者任一命中 `Drain::finish` 的新守卫臂，返回新的 `EXIT_INCOMPLETE = 4` / `status: "incomplete"`，`stop` 分别为 `events_dropped` / `stream_closed`。**尾部 `TurnCompleted` 不能掩盖被丢弃段里的拒绝或失败**。
- 不凭空补：缺口不被解释成审批拒绝，缺测用量不被补零。
- `Closed` 分支的文档明确限定为**防御性接口边界**（正常 `RuntimeHandle` 自持 broadcast Sender，正常组合不会经由它结束），不当作可达的生产故障。
- 顺带修复报告中的小项：JSONL **双换行**（`BoundedJsonlSink::write_line` 独占行边界）；不在终态时**先**提示 Core 取消在途回合，再等输出排空，使清理与（可能慢的）输出收尾重叠；返回的 writer **不再**在 writer 线程 close bound 之外同步 re-flush。

## 回归（红先，并用变异恢复法在本机复验）

新增 6 条测试：

| 测试 | 断言要点 |
|---|---|
| `cli::tests::headless_dropped_denial_followed_by_turn_completed_is_not_complete_success` | 丢掉拒绝事件＋尾部 TurnCompleted → 不得报完整成功 |
| `cli::tests::headless_closed_receiver_without_terminal_event_is_not_success` | 无终态的 Closed receiver 不得成功（防御接口用例） |
| `cli::tests::headless_jsonl_emits_each_event_with_a_single_newline` | 每个事件只写一个换行 |
| `cli::tests::bounded_jsonl_sink_appends_exactly_one_newline` | sink 独占行边界 |
| `main::guard_tests::guard_engages_then_restores_in_reverse_order` | 启用/逆序恢复序列 |
| `main::guard_tests::guard_restores_partial_state_after_early_failure` | 早退失败时回滚部分状态 |

**变异恢复法复验**：把 `Drain::finish` 的 `events_dropped || closed_without_completion` 守卫臂短接（等价修复前「没看到失败即成功」）→ 上述两条 headless 测试**均 FAILED**；还原后 `sha256sum -c` OK。

## 已执行验证

```
cargo test -p agent-tui          → 75 passed; 0 failed（含 guard_tests 2 条）+ real_binary_startup 2/0
cargo clippy -p agent-tui --all-targets -- -D warnings → 0
cargo fmt -p agent-tui -- --check → clean
```

## 未完成与限制（如实）

- **U6 未实现**：`session.rs` 仍有 20 处 detached `tokio::spawn`（`/focus`、`/task`、`/done`、`/continue` 等各自异步发送），**不保证按用户键入顺序送达**；`/checkpoint`、`/restore` 仍在输入循环内 `await`，慢磁盘会占住绘制循环；也未使用 Runtime 已有的带 `expected_task_id` 接口。A3 因此**只完成 U5**。
- 终端 guard 的证明是**可注入 backend 的状态机单测**；真实 PTY 下的早退/panic 端到端验证**本轮未执行**。
- `EXIT_INCOMPLETE = 4` 是新增出口码；现有 0/1/2/3 含义未变，但**依赖 headless 出口码的调用方/脚本需要知道 4 的存在**。
- U7 只覆盖 headless 路径；TUI 交互路径的事件缺口（对应 U3）不在本切片内。
- 未运行真实供应商请求、未跑 workspace 全量 CI。
