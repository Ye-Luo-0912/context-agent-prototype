# A2 落地回执：一份事件读模型与按任务 review（U3＋U4）

- 基线：`d92564bcfa41dda44f752e1e49e88a321abdf942`
- 提交：`crates/agent-tui/src/state.rs`、`session.rs`、`crates/agent-runtime/src/status.rs`
  - U3：`baa2ca70`（`tui+runtime: U3 one shared event fold, exact-once by identity (A2 part 1)`）
  - U4：`7224ec6d`（`tui: U4 review material belongs to one task, omissions are counted (A2 part 2)`）
- 依据：[REVIEW.md](REVIEW.md) U3/U4、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) A2

## 用户动作

`/status`、`/review` 与现场画面一致；重放或重复投递不改变结论；任务 B 的修改与失败不挂在任务 A 的完成头下。

## U3：一套共享事件折叠规则

| 问题（修复前） | 修复后 |
|---|---|
| 水位只跳过 `status_projection.fold`，**同一已覆盖事件仍继续改所有本地字段** | `apply_runtime_event` 先 claim `(RunId, seq)`；claim 被拒即**整体返回**，不产生任何变更 |
| 实时与重放走两套规则（重放只 `fold` + 补对话） | 折叠体抽到 `apply_event`，**重放调用同一函数** |
| `resync_projection` 只重建公共投影，本地字段漂移 | 先 `reset_event_derived_view()` 归零事件派生字段，再经共享折叠重建；输入草稿/滚动/面板开关/屏上审批等**纯界面状态不动** |
| 对话与 live 消息按**正文内容**去重（5 处） | 按**事件身份**去重：行用 `claim_message_row(run_id, seq)`，用户气泡用 `input_id`；内容不再参与身份判断 |
| `AssistantMessage` 覆盖「最后一条 assistant 行」，把两轮合并 | 只终结**本轮自己流式打开的那一行**（`streaming_row_open`） |
| `StatusProjection` 不折叠 `TurnCancelled` | 补上（加法）：清除 in-flight，单独计数，**绝不**当作完成或任务关闭 |
| 坏行/短读/序列缺口仍报完整并把最大 seq 当连续水位 | 标记 `view_partial` + 原因；**只在可验证连续前缀时才设水位**；`/status` 明示 PARTIAL |

边界：claim 集合是有界 FIFO（8192），被淘汰的老身份仍由连续水位覆盖，长会话内存不增长。

## U4：复核材料按任务归属，遗漏可见

- `ResultCard` 增 `task_id`；焦点切到别的任务时**归档**旧卡（有界保留最近 8 张）而不是继续追加，新任务开自己的卡 → 任务 A 的持久完成头不会盖在任务 B 的修改之上。条目（`CardChangedFile`/`CardCheck`/新增 `CardFailure`）各自带 `task_id`。
- `/review` 走 `AppState::review_card()`：优先当前任务卡，否则最近完成的归档卡，**永不混合两个任务**。
- 容量上限拒绝追加时**计数**（`omitted_files/checks/failures`）；`format_result_lines` 输出「N recorded, M FAILED, K not shown」与显式「…and K more were NOT shown」行；原先恒为 0 的 `len - cap` 溢出算术已删除。
- 快照写入：单写者 gate + 单调 `revision`（旧版本到达即丢弃）+ 临时文件 rename 提交；仍是 advisory 展示产物，不是运行权威。
- 所有新字段 `serde(default)`，既有 `result-card-latest.json` 仍可加载。

## 回归

新增 8 条测试：

| 测试 | 断言要点 |
|---|---|
| `a_redelivered_event_neither_double_counts_nor_reactivates_an_operation` | 重复投递已覆盖的 `ModelUsed`/`ModelStarted` 不重复计数、不复活旧操作 |
| `identical_replies_in_different_turns_stay_as_two_rows` | 相同文字不同 turn 保留两行 |
| `a_replayed_view_equals_the_live_view` | 实时消费 vs 遗漏后重放＋再投递全量事件 → tokens/status/busy/current_op 等价，且不重复追加行 |
| `an_unparseable_journal_line_keeps_the_view_partial` | 坏行 → partial＋原因，`/status` 出现 PARTIAL |
| `a_journal_gap_is_never_claimed_as_contiguous_coverage` | 序列缺口不设连续水位，且已折叠事件不被重复应用 |
| `agent-runtime::status::a_cancelled_turn_is_terminal_and_never_reads_as_completed` | 取消后 `in_flight=none`、`turns=0`、任务仍 awaiting review |
| `a_completed_tasks_header_never_covers_another_tasks_changes` | A 完成→B 修改并校验失败→review 属于 B、无 A 的完成头与 A 的文件 |
| `a_failed_check_beyond_the_card_cap_is_counted_not_hidden` | 第 33 个 check 失败 → `omitted_checks=1`、总数 33、review 明示遗漏 |
| `card_snapshots_are_versioned_and_committed_atomically` | 新卡胜出、无遗留 staging 文件 |

**变异恢复法复验**（sha256 校验备份，改完立即还原）：

| 变异 | 结果 |
|---|---|
| `claim_event` 恒返回 true（等价修复前无精确一次） | 重投递用例 **FAILED**，tokens `(1400,40)` 而非 `(700,20)` |
| 去掉 `streaming_row_open` 门控 | 相同回复用例 **FAILED**，1 行而非 2 行 |
| 关闭 partial 分支（恒取 else） | 两条 partial 用例 **FAILED** |
| `begin_card_for_task` 空操作 | 跨任务用例 **FAILED**（"the card must belong to the task being reviewed"） |
| 去掉 `omitted_checks` 计数 | 第 33 个 check 用例 **FAILED**（0 vs 1） |
| 去掉 `revision` 自增 | 快照用例 **FAILED** |

> 说明：这几条测试是**先写测试再实现**的（`identical_replies_in_different_turns_stay_as_two_rows` 在实现前即失败，暴露了 `AssistantMessage` 覆盖上一轮行的真实缺陷）；随后用变异恢复法在上面的最终代码上**复验**其可红性。

## 已执行验证

```
cargo test -p agent-tui          → 83 passed; 0 failed + real_binary_startup 2/0
cargo test -p agent-runtime --lib status:: → 5 passed; 0 failed
cargo fmt -p agent-tui -- --check → clean
cargo clippy -p agent-tui --all-targets -- -D warnings → 0
```

## 限制（如实）

- 未跑 `cargo test -p agent-runtime` 全量（该 crate 有并行会话在 `execution/`、`actor/` 的未提交改动）；只跑了 `status::` 模块与库编译。**workspace 全量 CI 未执行。**
- `apply_event` 的 fold 仍是「一次事件多处 mutate」的形态，只是收敛到单入口 + 精确一次；**没有**把各面板改成统一读取 `StatusProjection`（审查也明确说不能只做这一步）。
- 归档卡上限 8 张，超出即丢弃最旧；`/review` 只能看当前或最近一张，**不能按 TaskId 查询任意历史任务**。
- `view_partial` 只覆盖 journal 重放路径；**live broadcast 侧的 `Lagged` 缺口在 TUI 内仍未接入视图完整性**（该缺口的 headless 侧已由 U7 处理）。
- 结果卡快照仍是 advisory：写入失败只丢弃，不重试、不报错给用户。
