# W4 回执——取消不抹掉已知用量（V7）

- 状态：实现完成，待主会话验收提交（本回执撰写时未 commit，工作树基于 `b6ffc514`＋并行未提交改动）。
- 执行者：W4 工单（NEXT_TASKS.md 第四批，V7）。范围：`provider-openai/src/retry.rs`、`agent-contracts/src/runtime.rs`、`agent-runtime/src/actor/{model,tools,turn,mod}.rs`（仅取消/结算分支）、`agent-runtime/tests/turn/stream.rs`、新增 `agent-compose/tests/cancel_usage_settlement.rs`。
- 并行冲突声明：会话期间并行 Agent 在改 `context-simple`（engine/scope/tests＋新测试文件）、`agent-contracts/src/context.rs`、`provider-openai/src/lib.rs`（W3 mapper＋`endpoint_shape_tests.rs`）。本切片未触碰上述文件；`agent-runtime/src/actor/model.rs` 仅改模型 operation 结算 match 的取消臂（spawn 内 1707-1730 区域），未进入 S2a final-pack 区域（动手前后均核对 `git diff`，无重叠）。期间一次 `context-simple` 中间态编译错误（W1 的 `ScopeRetirementPermit` 半成品）按工单预案等待约 4 分钟后自行恢复，非本切片文件。

## 现场描述（HEAD `b6ffc514` 上）

反例链核实与审查报告一致：

- retry 循环三个 backoff-cancel 出口（`retry()` 自由函数、live/buffered 流模式）把已结算的 `known` 写进 `CallStage.known_usage`（仅 observer 通道）后返回裸 `AgentError::Cancelled`；`terminal_error` 对取消也无条件剥掉 usage wrapper。
- 生产 composer（`agent-compose/src/lib.rs::provider_from_env_with_timeout`）挂的是 `JsonlRetryObserver::from_env()`——未设 `OPENAI_RETRY_METRICS_FILE` 时 `append` 直接 return，`CallStage.known_usage` 无任何正式去向。
- Runtime 侧 `actor/model.rs:1710` 只匹配裸 `Err(AgentError::Cancelled)` → `OperationOutcome::Cancelled`（不携带用量）；用户先取消时代际隔离使该 completion 变 stale，`usage_already_accounted` fence 直接丢弃。

红-first 证明（修复前在 HEAD 上实际运行）：

| 测试 | HEAD 上的红 |
|---|---|
| `provider-openai::a_backoff_cancel_carries_the_known_usage_on_the_error` | `panicked: the reported usage must travel with the cancellation`（错误上 usage 丢失，只剩 observer 副本） |
| `agent-runtime(turn)::a_self_cancelled_round_with_known_usage_keeps_cancel_class_and_observed_row` | `panicked: the usage wrapper must not reclassify the cancellation`——HEAD 把 `FailedWithUsage{source: Cancelled}` 掉进 Failed 臂（class=Runtime），无 TurnCancelled。这正是工单禁止的「换返回值不调调用链」危害的实证 |
| `agent-runtime(turn)::a_user_cancelled_round_supplements_the_known_usage_once` | 账目只有 `[(0,0,Unknown)]`——stale fence 把已知用量丢掉 |
| `agent-compose::cancel_during_backoff_keeps_known_usage_and_cancelled_state` | 全链路（真实 `compose`＋`RetryingTransport`＋`JsonlRetryObserver::from_env`、metrics 环境变量不存在）账目只有 unknown 行：`left: [] right: [(90,30,Observed)]` |

## 修复：outcome 与 usage 正交（最终类型形状）

**AgentError 侧（携带者）**：不新增错误变体。取消的已知用量复用既有共享 wrapper `AgentError::FailedWithUsage { usage, source: Box<AgentError> }`，`source` 为 `Cancelled`；分类语义经既有 `failure_source()` 读取（无嵌套包装）。裸 `Cancelled` 匹配语义保持兼容——**无 usage 的取消仍返回裸变体**（`terminal_error` 改为统一走 `AgentError::failed_with_usage` 构造器：有已结算记录才包装，空信封永不出现）。

**Provider 侧**：`terminal_error` 取消不再无条件剥 wrapper；三个 backoff-cancel 出口统一改为 `return Err(terminal_error(AgentError::Cancelled, &mut known))`（`finish_failed` 原本就经 `terminal_error`）。`CallStage.known_usage` 降级为诊断副本——JSONL observer 仍是可选工件，不再拥有唯一事实。

**Runtime 侧（结算者）**：

- `OperationOutcome::Cancelled` 携带 `known_usage: Option<ModelUsage>`（`#[serde(default, skip_serializing_if)]`，旧序列化形态反序列化为 `None`）；`agent-runtime/src/actor/model.rs` 的结算 match 以 `Err(error) if matches!(error.failure_source(), AgentError::Cancelled)` 守卫臂（置于 Failed 臂之前）构造之——分类不变，用量正交随行。
- `tools.rs` Cancelled 臂（操作自行取消、非 stale）：先按真实计数结算 `ModelUsed`（observed 身份）并 `mark_usage_accounted`，再走原 `cancel_turn`（分类、安全屏障、代际隔离全部保持）；`turn.rs::emit_cancelled_usage_row` 增加同一 fence 检查——已入账的操作不再叠加 unknown 行。
- 用户先取消（backoff 等待中取消的主反例）：`cancel_turn` 屏障仍即时记 unknown 行（模型 future 可能无限挂起，不能延迟）；迟到的 stale `Cancelled{known_usage}` completion 经 stale 分支**一次性补记** observed 行（新 `usage_supplemented_ops` 有界 FIFO 防重复补记）。unknown 行按构造为零计数，补记不构成双重计费；「迟到的成功 completion 不补记」的既有语义（`cancel_and_late_completion_count_one_cost_exactly_once`）不变。

边界保持：未执行的重试不冒充已执行（provider/compose 两级测试断言调用数==1）；未知不补零（无 usage 取消仍走裸变体＋unknown 行；补记与结算都以 `has_any_reported()` 为门）；同次 SSE 累计快照不重复相加（mid-stream 用例断言 90+7=97 恰一次；`KnownAttemptUsage` 每真实 attempt 恰结算一次的机制未动）；未新建事件数据库（结算走既有 `RuntimeEvent::ModelUsed` 账目入口）；未用 `FailedWithUsage(Cancelled)` 冒充失败（守卫臂先于 Failed 臂，红测试实证了旧代码的危害方向已被堵住）。

## 改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/agent-contracts/src/runtime.rs` | `OperationOutcome::Cancelled { known_usage }` 字段＋文档 |
| `crates/provider-openai/src/retry.rs` | `terminal_error` 统一走 `failed_with_usage`；三个 backoff-cancel 出口；`CallStage.known_usage` 文档降级为诊断副本；两个既有取消用例更新断言（改名 `..._keeps_the_cancel_class_and_both_records` / `..._keeps_the_class_and_the_settled_usage`）＋新增红测试 |
| `crates/agent-runtime/src/actor/model.rs` | 结算 match 取消守卫臂（仅此区域） |
| `crates/agent-runtime/src/actor/tools.rs` | Cancelled 臂先结算 known usage；stale 分支一次性补记 |
| `crates/agent-runtime/src/actor/turn.rs` | `emit_cancelled_usage_row` fence；`mark_usage_supplemented`/`usage_supplemented` 辅助 |
| `crates/agent-runtime/src/actor/mod.rs` | `ActorState.usage_supplemented_ops` 有界 FIFO（与既有 account fence 同界 64） |
| `crates/agent-runtime/tests/turn/stream.rs` | 两个新回归（自取消分类＋用户取消补记） |
| `crates/agent-compose/tests/cancel_usage_settlement.rs` | 新增：Compose→Retry→Cancel 全链（生产 retry 组装形状、`OPENAI_RETRY_METRICS_FILE` 不存在断言、未执行重试不多计） |

消费者核对：工作区内对 retry transport 错误匹配裸 `AgentError::Cancelled` 的生产代码只有 `actor/model.rs:1710` 一处（eval/TUI/conformance/replay/host/process 均只匹配事件或无关通道）；compactor 的 cancel token 为新建永不对消，backoff-cancel 在该 lane 不可达。

## 实际命令与结果（本机，Windows Git Bash）

| 命令 | 结果 |
|---|---|
| 新测试（修复前，HEAD） | 4 红如上表——缺陷证明 |
| `cargo test -p provider-openai` | **155/155 绿**（含既有 success/give-up usage 并入回归全部不重开） |
| `cargo test -p agent-compose` | 15 个测试二进制全绿、0 失败（含新增全链用例） |
| `cargo test -p agent-runtime --test turn` | **146/146 绿**（含 `cancel_and_late_completion_count_one_cost_exactly_once`、`cancelling_an_in_flight_model_round_leaves_an_unknown_usage_row`、shutdown/steer/materialize 取消回归） |
| `cargo test -p agent-runtime --lib` | **421/421 绿** |
| `cargo test -p agent-runtime`（全部二进制） | actor 96、lib 421、turn 146、host 31、instance/其余全绿，0 失败 |
| `cargo test -p agent-contracts` | 183/183 绿（契约序列化兼容） |
| `cargo clippy -p provider-openai -p agent-compose -p agent-runtime --all-targets` | 三 crate 0 警告 0 错误（唯一 warning 在 `context-simple/src/engine.rs`——并行 Agent 的 W2 中间态文件，非本切片） |
| `rustfmt --edition 2024 --check`（本切片全部 8 个文件逐一） | clean（不用 crate 级 fmt，避免触碰并行 Agent 在飞文件） |

## 未做 / 取舍 / 限制

- **维护 lane 的取消用量**：maintenance/compactor 的取消仍走 unknown 行（`ModelCallRole::Maintenance`）。该 lane 的 compactor 请求持新建 token（`ModelBackedCompactor`），backoff-cancel 不可达；未为不可达路径扩形状。
- **迟到的成功 completion 不补记**：沿用既有设计（`cancel_and_late_completion_count_one_cost_exactly_once` 钉住），本切片只给「取消自身的已知用量」开正式通道——V7 的范围即此。
- **补记的账目形状**：用户取消路径最终账目为 unknown 行（0 计数）＋observed 补记行两行。数值上不双计（unknown 为零），但逐行计数消费方会看到 2 行/次取消；选择补记而非改写历史行，因为事件日志 append-only 且 unknown 行在屏障时刻是诚实结算。
- **真实供应商账单核对**：未做，不声称端到端费用数值正确性；结算语义由 typed 断言钉住。T8 条件实验不在本切片。
- 并行 Agent 的 context-simple 中间态一度使 `cargo test -p agent-runtime --lib` 编译失败，等待恢复后重跑全绿；期间未触碰其文件。未 commit。
