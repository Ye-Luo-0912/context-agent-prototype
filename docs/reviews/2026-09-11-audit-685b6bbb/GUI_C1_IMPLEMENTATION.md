# C1（GUI-1）实施与验证回执

- **切片**：GUI 线首切片 C1 = GUI-1「生产入口与异步隔离」
- **对应发现**：F05（P1）、F07（P2）
- **基线**：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d`（审查固定提交，工作树无未提交源码改动时起始）
- **日期**：2026-09-11
- **范围**：`apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`、`clients/dotnet/Agent.Client.Tests/WorkbenchReviewTests.cs`
- **未触碰**：Rust crate、协议 DTO、`clients/dotnet/Agent.Client` 库本体、`command.rs`、compose 入口

## 1. 基线复核（实施前，本工作树）

审查为源码推导（审查环境无工具链，未运行本地测试）。本切片实施前在 `685b6bbb` 工作树逐条静态复核，三项均成立：

| 发现 | 位置 | 复核结果 |
|---|---|---|
| F05 | `MainWindowViewModel.cs:592` | `BuildTransport()` 的 switch 只有 `UnixSocket` 分支建 UDS，其余（含 `RealHost` 默认）一律 `new NamedPipeTransport(Endpoint)`。`AgentTransports.DefaultLocal()` 已平台感知但生产入口未调用。 |
| F07 | 四条只读路径的 catch | `LoadTaskDetailAsync`/`RefreshChangesAsync`/`ReadArtifactAsync`/`RefreshContextAsync` 成功路径调用 `IsCurrentEra`，但 catch 分支无任何校验即写「…读取失败（unavailable）。」 |
| F07（第二半） | `SelectedTask` setter | 仅 `if (value is not null) _ = LoadTaskDetailAsync(value.TaskId);`，无 selection 代次，旧任务详情结果与新选择无法区分。 |

## 2. 实施

### F05：RealHost 平台感知选择

```
internal IAgentTransport BuildTransport() => BuildTransport(
    (TransportKind)TransportIndex, Endpoint, OperatingSystem.IsWindows());

internal static IAgentTransport BuildTransport(
    TransportKind kind, string endpoint, bool isWindows) => kind switch
{
    TransportKind.WindowsNamedPipe => new NamedPipeTransport(endpoint),
    TransportKind.UnixSocket => new UnixDomainSocketTransport(endpoint),
    _ => isWindows
        ? new NamedPipeTransport(AgentTransports.DefaultPipeName)
        : new UnixDomainSocketTransport(
            AgentTransports.DefaultSocketPathFor(Environment.CurrentDirectory)),
};
```

- 显式两种选择保持用户 endpoint 原样；`RealHost`（及防御性的 `LayoutPreview`）走 OS 决策。
- 决策抽出为可注入 `isWindows` 的静态重载：**该缺陷只在非 Windows 显现**，本机为 Windows，若只断言运行时实例类型，测试在 Windows 上会空洞通过。注入式重载使 Linux 分支在任何宿主可被证伪。

### F07：统一请求 era 守卫

```
private readonly record struct RequestEra(long Selection)
{
    public static readonly RequestEra Unbound = new(0);
}

private long _selectionSequence;

private RequestEra CaptureRequestEra(long selection = 0) => new(selection);

private bool IsRequestCurrent(RequestEra era, IAgentConnection connection) =>
    IsCurrentEra(Volatile.Read(ref _generation), connection)
    && (era.Selection == 0 || era.Selection == Volatile.Read(ref _selectionSequence));
```

- 四条只读路径统一：await 前 `CaptureRequestEra(...)`，**成功与失败两条路径**分别 `IsRequestCurrent` 校验后才写面板；失败且已过期时直接 return（不再覆盖新面板）。
- `SelectedTask` setter 每次变更（含置空）`Interlocked.Increment(ref _selectionSequence)`；`LoadTaskDetailAsync(taskId, selection)` 绑定该序号。
- 消解了原先 `LoadTaskDetailAsync` 的「同步抛错」与「await 抛错」两段重复 catch——现在只有一条 await 路径、一处 catch。

### F07 第三处（续做复核新增）：era 作用域 UI 缓冲

审查原文未点名，但在本工作树复核 F07 时发现的同类泄漏：

- `DrainPendingUiEvents` 把合并后的 `deltaText` 直接 `AppendOutput(deltaText)`，**没有任何 era 检查**——`HandleEvent` 的 `ReferenceEquals(_connection, connection)` 只保护逐个 event，保护不到这段合并文本。
- `DisconnectCoreAsync` 清理了各面板，但**不清理** `_pendingUiEvents` / `_pendingUiDelta`。

后果：旧连接的 `model_delta` 在断开前被合并进 delta 缓冲，其排队的 drain 在断开后运行，旧正文被刷进新会话的输出面板。

修复：

- 新增 `_deltaEraConnection` 字段：在 `EnqueueDeltaForUi` 排队时戳上产出连接；`DrainPendingUiEvents` 仅在 `ReferenceEquals(_connection, deltaEra)` 时 flush 该段文本。
- `DisconnectCoreAsync` 在 era 边界统一清理两个缓冲与戳。

```csharp
lock (_handoffGate)
{
    _pendingUiEvents.Clear();
    _pendingUiDelta.Clear();
    _deltaEraConnection = null;
}
```

## 3. 验证

所有命令在本工作树执行（Windows，dotnet SDK 10.0.301）。

| 检查 | 命令 | 结果 |
|---|---|---|
| 桌面构建 | `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj` | 成功，0 警告 0 错误 |
| 客户端测试 | `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj` | **95/95 通过**（基线 91 ＋ 本切片新增 4） |

### 反例先行（证明回归有效）

新增 4 项回归**均先在「回退修复」的代码上复现失败**，再随修复转绿：

| 回归 | 回退后 | 修复后 |
|---|---|---|
| `RealHost_default_selects_the_platform_transport` | FAIL（Linux 分支返回 NamedPipe） | PASS |
| `Late_era_failure_does_not_clear_a_fresh_panel` | FAIL（旧 era 失败清空新面板） | PASS |
| `Slow_detail_for_a_previous_selection_cannot_paint_the_new_one` | FAIL（A 的晚到详情覆盖 B） | PASS |
| `A_retired_eras_buffered_delta_never_reaches_the_new_panel` | FAIL（旧 era delta 刷进新面板） | PASS |

四项测试的语义：

1. **默认传输平台决策**：同时断言运行时实例与 SDK `DefaultLocal()` 一致，并显式驱动 `isWindows:false`（必须 UDS）与 `isWindows:true`（必须命名管道）；再验证两种显式选择原样保留 endpoint。
2. **晚到 era 失败不清面板**：旧连接上挂起 detail/changes 读取 → 断开 → 装新连接并取到有效面板 → 旧请求以异常结束 → 断言新面板内容逐字不变、无「读取失败」。
3. **选择切换**：选中 B（取到卡）→ 选 A（挂起）→ 再选 B → A 的晚到结果落地 → 断言面板仍是 B、不含 A。
4. **退役 era 缓冲**：冻结 UI 的 dispatcher → 旧 era 写入 `model_delta`（仅入缓冲）→ 断开 → 手动执行排队的 drain → 断言输出面板不含旧正文。

### 未验收（如实记录）

- **真实 Linux 宿主端到端连接未跑**：本机为 Windows，未做真实 UDS `ConnectAsync → BuildTransport` 链路。非 Windows 分支由决策表与注入式重载覆盖，属构造性验证，不是实机连接验收。
- 审查同时要求的「统一 WorkspaceIdentity/endpoint 展示与解析、消除相对/绝对路径歧义」属 **PLATFORM-3** 范围，本切片只修默认入口本身，未扩张。
- F07 审查建议的「下沉为客户端/VM 小型统一助手」已按 VM 内统一助手实现（`RequestEra`＋`IsRequestCurrent`），未改客户端库，未复制多套布尔状态。
- 真实 provider 场景照旧 **NOT_RUN**。
- 未提交、未推送、未跑远端 CI；关闭按既有规则待远端 CI run 记录确认。

## 4. 与并行线的边界

本切片仅改 GUI 与 GUI 测试两文件。同期工作树中的 `crates/agent-runtime/src/execution/body_cache.rs`、`prompt.rs`、`agent-contracts/src/context.rs` 等改动属并行 **CORE-1** 线，与本切片无重叠、未互相覆盖。
