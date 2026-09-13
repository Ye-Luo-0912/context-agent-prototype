# GUI-3 第二片实施回执：继续动作的任务显式化＋取消请求状态机（F06 后续、任务书 GUI-3 第 2/3 条）

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 切片归属：[NEXT_TASKS.md](../../NEXT_TASKS.md) C 线 **C3（GUI-3）** 第二片；承接同日首片（F06 exact-request 查询消费半，见 [GUI3_EXACT_REQUEST_IMPLEMENTATION.md](GUI3_EXACT_REQUEST_IMPLEMENTATION.md)）。
- 改动范围：`apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`、`apps/Agent.Desktop/MainWindow.axaml`、新增 `clients/dotnet/Agent.Client.Tests/WorkbenchContinueCancelTests.cs`。**不改 Rust、不改协议 wire、不改宿主、不改客户端库。**

## 用户能做什么

1. **继续不隐式认领选中任务**：wire 契约里 `work.continue` 无 task 绑定字段（双侧均为空 body，只继续运行的活动任务）。工作台现把这一事实说明白——回执后核对回执 task 与操作员当前选中任务：一致或不选时正常报告；不一致时明说「继续的是运行的活动任务，不是当前选中的任务 X——继续动作始终跟随活动任务，不隐式切换选择或新启任务」。按钮文案改「继续活动任务」，输入区常驻提示「新目标永远是新任务」。
2. **取消分「请求已发出」与「可信停止」**：新增 `CancelRequestPhase` 状态机并露出到状态条（`CancelStateText`）——点击取消即显示「已发出，等待服务端确认；在可信确认前不视为已停止」；只有类型化 ack 把它升级为「已确认取消（屏障已过）」（`Cancelled`）或事实性「当前没有活动轮次」（`NoActiveTurn`，不是失败）；结果不可确定（断线/超时）时保持「不可确定」警告——该轮可能已停止也可能未停止，副作用以快照与事件为准，不自动重发取消。
3. **快照重推导与断开失效**：未决（Requested/Unknown）阶段由下一份可信快照按类型化事实解除并注明「以快照为准」；取消在途时快照不越权重推导（`_cancelInFlight` single-flight）；断开连接把未决状态整体作废（「已断开：未决的取消请求状态失效；重连后以快照为准」），晚到的 ack/失败经 era 守卫不得在断开后的界面上复活状态。
4. **重连 watermark 接续（任务书第 4 条）**：核对后确认全部复用既有机制——`ResumableSession` 订阅先于快照（B1 SNAP-GAP）、快照 watermark 去重 durable 事件、有序复位边界、GUI `Resynced` 处理器应用快照并按需重启事件泵。本片无新增代码，不另起事件真值源。

## 实现要点

- `CancelAsync`：进入即 CAS `_cancelInFlight`（在途去重）→ `Requested` → await → 按 ack/异常迁移，三处回执路径全部经 `IsCurrentEra(generation, connection)` 守卫——退役 era 的结果不改写状态。
- `ApplySnapshot`：在 `_cancelInFlight == 0` 且阶段 ∈ {Requested, Unknown} 时重推导为 Idle，文案直接引用快照布尔（运行仍在进行 / 没有进行中的运行），不引用会自我嵌套的展示串。
- `ContinueAsync`：发出前捕获 `SelectedTask?.TaskId`，回执后比较并拼接注意行；无选择时不加。
- `DisconnectCoreAsync`：`SetCancelPhase(Idle, "已断开…")`。

## 回归（新增 `WorkbenchContinueCancelTests`，5 项，均在只加状态面、无转移逻辑的代码上先复现失败再转绿）

1. `Continue_reports_which_task_it_continued_and_flags_a_selection_mismatch`——选中 A、回执 B：输出同时含两个 id 与「不是当前选中的任务」；无选择时不加注意行（旧实现只报 task id：红）。
2. `Cancel_shows_requested_until_a_typed_confirmation`——请求挂起时 `Requested`＋「已发出/等待」；在途期间快照不得改变阶段；只有 `Cancelled` ack 升级为「已确认取消」（旧实现无状态面：红）。
3. `Cancel_without_an_active_turn_is_a_fact_not_a_failure`——`NoActiveTurn` 是事实非失败。
4. `Cancel_unknown_outcome_keeps_the_warning_until_a_snapshot_rederives_it`——未知结果保持「不可确定」警告；下一份快照以「以快照为准」解除回 Idle。
5. `Disconnect_retires_an_open_cancel_request_state`——断开作废未决状态；在途取消随后对死 era 结算，不得复活状态。

## 实际检查

- `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`：**106/106 通过**（首片后 101＋本片 5；含真实宿主链 HostChainTests）。
- `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`：0 警告 0 错误。
- `python scripts/doc_consistency.py`：OK（13 live docs）。
- 先红后绿在本会话内实际执行（filter 单跑：实现前 5 失败/0 通过 → 实现后 5/5）。

## 未验收（如实记录）

- 未提交、未推送、未跑远端 CI；真实宿主上的取消/继续交互未人工走查（HostChainTests 真实宿主链在套件内通过，但取消状态机的人工 GUI 走查未做）。
- 「追加指令绑定选定 task」的**协议级**绑定（continue 携带 task id）属平台线契约增量，本片按现契约做 GUI 侧诚实呈现，未改 wire。
- GUI-3 全部条目完成后的阶段验收（统一用户旅程第 5/6 步）待真实宿主场景。
- 真实 provider 场景照旧 NOT_RUN。
