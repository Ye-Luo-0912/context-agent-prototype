# F5 · 长流程后端控制面（宿主路由＋SDK 镜像）实施回执

**基线：** `main` @ `7a536468`（M18 三轮全部在 main，CI run `34742686207` 七 job 全绿）。
**范围：** 长流程后端审查的 F5 —— 把 `RuntimeCommand` 已有的「任务内转向 / 激活 / 挂起 / 检查点恢复」语义接到正式宿主路由与 .NET SDK，并让多入口操作带精确身份。**不含** GUI 功能开发（只保证 DTO 编译兼容）、F1–F4、多 Agent 编排、插件 UI。

## 用户现在能做什么

一个**非 GUI** 客户端（host e2e 或 .NET SDK）能在**同一个 RuntimeActor** 上走完整闭环：

**start → steer → suspend → activate → continue → checkpoint → restore → verify**

每个精确操作都带它期望的身份，**比较发生在 actor 内部的同一次串行化步骤里**，不是「先读快照再无条件下命令」。

## 六条最小闭环逐条

1. **新提交 vs 任务内转向。** `work.steer` 与 `work.submit` 是两条路由：转向落在运行中的那个任务上，**永不创建任务、永不改焦点**。回执类型化——`Applied`、`Queued`（进了运行回合唯一的修正槽，已受理但尚未执行）、`Rejected`＋原因（`ExpectedTaskMismatch` / `NoActiveTask` / `QueueFull`）并附当前活动任务 id，让调用方改目标而不是重发到错误任务。
2. **激活/挂起走同一个 actor。** `work.activate` / `work.suspend` 驱动既有 `TaskManager`，**没有第二套任务表**；激活回执说明它顶替了谁、以及是否根本没动；挂起绝不被表述成完成。
3. **继续/取消带期望身份。** continue 的 `expected_task_id`、cancel 的 `expected_task_id`/`expected_turn_id` 在 actor 内与实际值比较：不匹配则**不起回合 / 不取消任何东西**，并报告真正在跑的是什么。契约还拒绝「不匹配却带 Cancelled ack」的应答——那会声称一次从未发生的停止。
4. **只有正式检查点。** 新 `RuntimeCheckpointPlane` 把跨面 capture/restore 事务（actor＋context＋宿主能力面＋持久 Core 授权标记）从 `RuntimeInstance` 抽出，`RuntimeInstance` 转为委托它——**不存在第二条会漂移的流程**。`work.checkpoint` / `work.restore` 用同一个 plane；actor-only dump 仍是 crate-private，客户端无法把半份产物当检查点持久化。产物名是**store 内文件名，永不是路径**（穿越尝试在打开任何文件之前被契约拒绝）。
5. **共享且经校验的运行配置。** `agent_compose::parse_max_model_rounds` 成为模型轮预算的唯一严格校验器；宿主新增 `--max-rounds`，TUI 改为委托同一规则。快照报告 `effective_config`：上下文策略、**从内核实际权威读到的**有限模型轮预算＋它是否由操作员显式设置、维护预算/超时、provider profile digest、缓存模式、read-only。
6. **状态查询回答运维问题。** 快照还带 `continue_readiness`（类型化原因：`Ready` / `TurnRunning` / `NoActiveTask` / `RecoveryRequired` / `CleanupInFlight` / `NoRetainedDirective` / `DirectiveMayBeTruncated`）与 `store_backpressure`——不需要 GUI，也不解析任何文字。

## Wire 兼容

期望字段与新快照事实都是 skip-serialized：**未指定期望的请求、以及真的发生了的继续**编码出与历史逐字节相同的 JSON。全部既有跨语言金样未改动即通过，两侧各有专门测试钉住这一点。

## 接线时发现并修掉的一个真缺陷

.NET 客户端在**写出任何字节之前**自己拒绝的请求，此前被包成 `AgentUnknownOutcomeException`——告诉调用方「这次修改可能已送达」。伪造的「未知」和伪造的成功一样有害：它会把操作员推向重新核对甚至重发从未离开本进程的工作。现在 pre-send 契约违规标记 `RequestNotSent` 并原样传播。

## 实际执行的验证

本环境安装了 Rust 1.97.1（与 CI 一致）与 .NET SDK 10.0.301，以下都是真实运行结果：

| 命令 | 结果 |
|---|---|
| `cargo test -p agent-host` | lib 9、host_config 3、**host_e2e 11**（含新增 F5 UDS 闭环）、host_restore 3 全绿 |
| `cargo test -p agent-runtime --test actor` | **93/93**（含新增 7 项 steering/身份回归） |
| `cargo test -p agent-platform-protocol` | lib **54**＋work_fixtures **15**（既有金样零改动通过） |
| `dotnet test clients/dotnet/Agent.Client.Tests` | **131/131**（含真实 `agent-host` 二进制上的无头闭环） |
| `dotnet build apps/Agent.Desktop` | 0 warning / 0 error |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets` | 0 warning |
| `python scripts/doc_consistency.py` | OK（13 live docs） |

**红检查（去掉修复后测试必红）：** 移除 steer 的身份比较后，host e2e 闭环失败——且**先失败的是契约校验器**，它拒绝把一次修正报告成落在请求未点名的任务上（防线在运行时之外还有一层）。

## 未验收 / 如实记录

- 真实 provider 未调用：闭环用 `AGENT_DEMO=1` 的脚本化 demo 模型（工具与编排是真的，模型不是付费端点）。真实成本/质量对照照旧属 COST-5，NOT_RUN。
- GUI 只做到编译兼容：桌面工作台没有为转向/激活/挂起/检查点新增界面；`ContinueAsync`/`CancelCurrentTurnAsync` 保持「跟随活动任务」的既有语义（GUI 消费面属 C 线）。
- 转向没有幂等账本：`work.steer` 不带 `client_request_id`，因为运行时没有 steering 收据可去重。SDK 因此**只发一次**，丢回执即 unknown，由调用方读快照决定——不自动重发。
- 跨重启提交确认边界不变（256 条进程内窗口，不承诺 exactly-once）。
- 远端 CI 以 run 记录为准，不由本地结果外推。
