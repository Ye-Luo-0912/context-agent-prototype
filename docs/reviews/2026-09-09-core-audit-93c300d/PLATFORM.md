# 平台接入审查：一致性、幂等与背压

基线：`93c300d9b222ea9720579b86ac273e945f1964bc`。

本报告确认 4 项问题（2 个 P1、2 个 P2），重点是状态一致性、提交身份和端到端背压。本代理没有修改生产代码，没有运行 cargo、全仓测试或真实 provider。

**证据保存说明：** 最初的探针源码、构建工件和报告位于 `target/review-2026-09-09/platform/`。本轮后续现场检查确认整个 `target` 目录已不在磁盘，HEAD 仍为上述基线，删除原因尚未确认。本文依据本轮真实工具调用及其输出记录重建；没有重跑构建。下文保留当时执行的命令和实际观察，但原探针文件当前不可用，该命令不能直接依赖现存工件复现。

## P1：合法提交目标不能被 .NET 快照接受，重连持续失败

**不变量：** 合法提交后，所产生的权威状态必须属于客户端可解码的状态集合，即 `accepted_goals ⊆ snapshot_goals ∩ task_detail_goals`。一次有效用户输入不能破坏后续握手。

**当前源码：**

- Rust `crates/agent-platform-protocol/src/work.rs:45-54`：submit 和 snapshot 的字符上限均为 200,000；UTF-8 总字节上限另行约束。
- .NET `clients/dotnet/Agent.Client/WorkDto.cs:27-33`：submit 上限 200,000；但 `:304-306`、`:343-355` 的 snapshot 上限仍为 2,000。B3 task detail 在 `:587` 复用该较小上限。
- 实际宿主路由 `crates/agent-runtime/src/platform/work.rs:485-508` 原样复制任务和焦点 goal。
- `clients/dotnet/Agent.Client/AgentConnection.cs:219-239` 对响应内容校验失败后 fault 整个连接；`ResumableSession.cs:169-185` 必须取得有效快照才能完成重连握手。

**实际反例：** 2,001 个 ASCII 字符的 goal 通过当前 C# submit 验证，并由 loopback scripted peer 返回 Accepted；随后两次 Snapshot 都抛 `AgentContractViolationException: is 2001 chars, above the 2000 char bound`，`connected=False`。2,001 字节远小于共同的 262,144 字节上限，因此并非 UTF-16/UTF-8 边界误差。Rust 合法性及其原样投影为源码证明，客户端 fault/reconnect 行为为实际 wire 探针观察。

**工程影响与修复方向：** 任一仍可见的合法长任务会使正式 .NET/GUI 快照和重连反复失败。统一双方边界，并补 Rust host → .NET 的 2,001 字符 submit/snapshot/reconnect/task_detail 回归；不要仅验证拒绝超大输入。

## P1：提交结果未知前后丢失幂等键，同目标重试成为新受理

**不变量：** 同一逻辑提交在 `Pending(k)` 和 `Unknown(k)` 状态下必须保留同一个键 `k`，直至有相关联的受理/拒绝事实。普通状态快照不携带该请求的受理证明，不能把“未看到任务”提升为“请求没有执行”。

**当前源码：**

- `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs:773-779` 在等待提交回执之前保存键。
- 所有 `ApplySnapshot` 都在 `:678` 调用 `ResolveOutstandingSubmitFromSnapshot`；`:736-749` 无条件清键，既不区分 Pending/Unknown，也不按提交身份关联。
- `:790-795` 的通用异常分支同样清键。
- `clients/dotnet/Agent.Client/AgentConnection.cs:180-181,219` 已发完整帧后的回执超时抛 `OperationCanceledException` 派生异常；`ResumableSession.cs:337-342` 明确将其排除于 Unknown 包装之外。
- Runtime `crates/agent-runtime/src/actor/turn.rs:145-167` 只按 `client_request_id` 命中 `AlreadyAccepted`；不同键在空闲后经过 `:173-206` 再次进入 `begin_applied_turn`。

**实际反例一：** `Pending(k) → 普通刷新返回旧空快照 → Pending(无键) → 回执丢失/Unknown → 用户同目标重试(k')`，且 `k' ≠ k`。源码探针通过正常 `RefreshOnceForTestsAsync` 路径复现，刷新时原 submit Task 仍未完成；最终 `calls=2, same id=False`。

**实际反例二：** scripted wire peer 已读到一个完整 submit 帧，但不回执；客户端实际抛 `TaskCanceledException`，`unknown typed=False`，连接仍为 true。GUI 对这一异常清键；用户同目标重试仍为 `calls=2, same id=False`。

**工程影响与修复方向：** 同一未重启的宿主原本可以凭旧键返回原受理结果；换键后，原轮次空闲时会再次提交用户输入，可能重复其副作用。这里证明的是重复进入新受理/新轮次的路径，未运行真实副作用/provider。应保留 Pending/Unknown 身份，以关联回执解除；分别表达发送前取消、发送后等待取消/超时和 host incarnation/幂等窗口不可证明等情况。现有按同名 goal 清理的走查不能证明请求受理，因为快照还过滤 completed 任务（`platform/work.rs:488`）。

**必要回归：** 提交与刷新并发后丢回执；同宿主已发帧超时后显式重试。两者均应验证原键与原受理身份保持。

## P2：session 事件队列溢出后，快照和底层重连无法恢复事件

**不变量：** 恢复要么重新建立完整可用的会话，要么对外保持明确终态；不能把“命令连接活着”当作“事件接入已恢复”。完成的 Channel/Task 是单调终态，清空内容不会重新打开它。

**当前源码：**

- `clients/dotnet/Agent.Client/ResumableSession.cs:267-281` 在 session 队列拒绝 durable 事件时执行 `_events.TryComplete(error)`，退出泵但不 fault 底层连接，也不将整个 session 标为终态。
- `_events` 是 `:64,79` 创建的永久队列；重连只在 `:214` 调用 Clear。
- `clients/dotnet/Agent.Client/BoundedEventQueue.cs:162-175` 的 Clear 正确保留 completion 状态；`:115-117` 拒绝向 completed 队列写入。
- GUI `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs:501-505` 只打印“从快照重建”提示并退出消费者。

**实际反例：** 容量 2 的 session 依次收到 3 个 durable 通知，确保底层源队列在发送间隔内已排空。排空 session backlog 后 Completion fault，而 `IsConnected=True`。Snapshot 随后成功，但 Completion 仍 fault。主动断开底层连接并 Snapshot 重连后，观察 `connections=2, connected=True`，新事件仍无法交付。

**工程影响与修复方向：** 出现“可操作、可刷新但事件永远不再到达”的半恢复会话；审批和输出只能依赖轮询或人工重建整个 session。应明确选定整个 session 终态并让消费端重建，或设计可恢复的会话事件边界；不能靠 Clear 复活一个已完成的 Reader。必要测试是 session-level overflow → 恢复 → 新通知真实到达；现有单独 Completion 和普通 reconnect 测试没有覆盖这一组合。

## P2：每事件一次 UI.Post 绕过事件队列和文本上界

**不变量：** 端到端保留量是所有队列之和，单独限制源队列和已渲染文本并不限制中间 dispatcher。若 UI 消费速率为零，现有递推是 `Q_next = Q + arrivals`，没有上界。

**当前源码：**

- `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs:483-494` 从有界源立即取出每条通知，并为其创建一个 `_ui.Post` 闭包。
- delta 直到 UI 回调执行时才在 `:521-524` 进入 DeltaCoalescer，已渲染的 400 行/64KiB 限额也在这之后生效。

**实际反例：** 容量 8 的有界源逐条送入 10,000 个 1KiB model delta，dispatcher 暂停执行。最终 `queued UI callbacks=10000, bounded source backlog=0, rendered output bytes=0`。这里测的是排队数量，未把它冒充 OS 内存采样。

**工程影响与修复方向：** UI 繁忙时，待执行回调及其 payload 持续累积；渲染文本与客户端 channel 的限额均不能抑制。应在投递 UI 之前合并/限流或采用有界批量交接，保留 durable 事件的明确恢复语义。必要回归是慢/暂停 dispatcher 下的 pending UI work 上界；现有 InlineUiDispatcher 与直接 AppendOutput 的测试仅覆盖渲染之后。

## 实际执行与记录中保留的输出

当时的 `PlatformProbe.csproj` 直接链接当前客户端、ViewModel、Infrastructure 和 Fixture 的 C# 源码，目标框架为 net10.0，使用 Avalonia 11.3.20 包。`Program.cs` 使用测试连接、正常刷新测试缝、反射调用实际 SubmitAsync 和暂停 dispatcher；`WireProbes.cs` 使用 loopback TCP scripted peer 驱动实际 AgentConnection/ResumableSession。它们均未连接真实 Rust 宿主或 provider。

当时最终成功的命令如下；**原探针文件及工件现已缺失，本节只记录历史执行，不承诺该路径当前可复现。**

```powershell
dotnet run --project target/review-2026-09-09/platform/PlatformProbe.csproj --no-launch-profile
```

工具返回退出码 0。仅有 harness 未使用 `MetricsSession.SnapshotFactory` 测试缝的 CS0649 警告。早期 harness 相对路径及 mock UUID 写法错误已在该成功运行前纠正，不是产品故障。

以下为本轮成功执行记录中的关键原始输出，省略随机生成的任务 UUID 和重复中文提示：

```text
before concurrent snapshot: first request pending=True, outstanding preserved=True
after concurrent snapshot: first request pending=True, outstanding null=True
after unknown response: outstanding null=True
explicit same-goal retry: calls=2, same id=False
timeout then explicit same-goal retry: calls=2, same id=False
wire long goal submit=Accepted, goal chars=2001
wire snapshot attempt 1: AgentContractViolationException, connected=False, invalid work.snapshot.task.goal: is 2001 chars, above the 2000 char bound
wire snapshot attempt 2: AgentContractViolationException, connected=False, invalid work.snapshot.task.goal: is 2001 chars, above the 2000 char bound
wire accepted-frame timeout: TaskCanceledException, unknown typed=False, host submit frames=1, connected=True
session queue overflow: AgentContractViolationException, connected=True
snapshot after overflow: succeeded, event completion faulted=True
reconnect after overflow: connected=True, connections=2, event completion faulted=True, new event delivered=False
paused UI after 10000 model deltas: queued UI callbacks=10000, bounded source backlog=0, rendered output bytes=0
```

## 覆盖与限制

- 重点沿调用链阅读：`agent-host` framing/session grant/event forwarder/dispatch/shutdown/UDS 与 named-pipe poll/cancel，main restore/config；protocol `work.rs` 与 B3；runtime `platform/work.rs` 实际只读/修改路由；.NET AgentConnection/ResumableSession/BoundedEventQueue/WorkDto/Envelope；GUI connect/refresh/submit/event/lifecycle。
- 阅读相关现有测试：EventStreamTests、ClientSafetyTests、WorkbenchLifecycleTests、RestoreWalkthroughTests，以及到达的 B3 fixtures/测试。未声称这些现有测试已在本轮重跑。
- 轻审：TUI 共享 work 入口、main/CLI lifecycle/lag、state resync/render；replay main/recovery/barrier/trace-read；dist.ps1 与 dist.sh。未运行真实宿主/TUI/replay、Linux/UDS、打包、全仓测试或 provider 场景。
- 定向确认后跳过已修问题：receiver 保留、subscribe→snapshot、live-only delta 分流、Queue Completion、grant guard/revoke、cancel-all 不先清表、retyped 保留原 work、正式 checkpoint 解码入口。
- B4/N8 的宿主 MCP/plugin 配置，以及 C3 的 B3 GUI 消费接线，是已列明未完成范围，不作为新缺陷。
- Workspace `after_tx` 游标重复的真实探针由 io-tools 代理维护，不重复报告/实验。其全日志同步扫描与协议字段预算候选已交给主代理交叉核对；本报告不把它们列为第五项已确认发现。
- 用户要求从数学/工程角度补充后已停止新增实验；此次仅在 docs 重建已有报告，未重建或重跑已丢失的探针。
