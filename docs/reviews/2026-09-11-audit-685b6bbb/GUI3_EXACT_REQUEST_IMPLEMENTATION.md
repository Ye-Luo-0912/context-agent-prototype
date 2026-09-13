# GUI-3 首切片实施回执：未知提交由 exact request 账本查询解除（F06 消费半）

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 对应缺陷：2026-09-11 执行者视角审查 **F06（P1 → PLATFORM-1＋GUI-3）** 的 GUI 消费半；切片归属 [NEXT_TASKS.md](../../NEXT_TASKS.md) C 线 **C3（GUI-3）**。
- 改动范围：`apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`、`apps/Agent.Desktop/Fixture/FixtureAgentConnection.cs`、`clients/dotnet/Agent.Client/{IAgentConnection,AgentConnection,ResumableSession}.cs` 与 `Agent.Client.Tests` 三个测试文件。**不改 Rust、不改协议 wire、不改宿主**；协议/运行时/宿主侧（`work.submit_result` 路由、五种 disposition、payload digest、宿主 dispatch 与 e2e）由并行 PLATFORM-1 线在同一工作树落地，本片只消费。

## 用户能做什么

提交因断线/超时丢失回执后，工作台不再用「快照里有没有同名目标任务」猜测结果——它自动用 `work.submit_result` 精确查询受理账本（client_request_id＋内容摘要），并按账本原话分型呈现：

- **Accepted / AlreadyAccepted**：该精确请求确已受理（报出 task id），未知解除；
- **KnownRejected**：同一 id 曾以不同内容提交——这次提交从未被受理，id 已死并被释放（原内容摘要前缀作为证据展示，不搬运旧 goal 文字）；
- **Unknown / Expired**（跨重启无证据/证据被淘汰）：**保持未知**，不自动重发；同一目标再提交仍用同一身份幂等重试，账本核对随每份快照继续。

## 删除的缺陷逻辑

`ResolveOutstandingSubmitFromSnapshot`（整方法删除）：先清 outstanding request id，再以 snapshot 任务列表中是否存在相同 Goal 宣布「已核对/解除」。同名旧任务不是本 request id 的证据；列表有界（200 条渲染）或 goal 超出快照展示上限时「没找到」也不是未受理证明。`_outstandingSubmitGoal` 保留但只用于计算查询的 payload digest，**永不参与任何匹配**。

## 实现要点

1. **解析路径 `ResolveOutstandingSubmitByQueryAsync`**（触发：每份成功应用的快照＋每个新记录的未知回执；single-flight `_submitResultQueryInFlight`）：
   - 只读 `SubmitResultAsync(id, SubmitPayloadDigest.Compute(goal))`；
   - **era fencing**：捕获 connection＋generation，答案仅在连接仍是 live 且回显 `client_request_id` 与所查 id 一致时应用（退役 era 的或错配的回执不是证据；下一份快照会再问）；
   - Pending（仍在等原回执）不查询——R07 语义不变：同目标重试必须留在同一受理身份上；
   - Unknown/Expired/查询失败经 `ReportOutstandingResolutionOnce` 每 id 只报一次（默认 10 秒兜底刷新会持续重问，输出面板不被刷屏）；确定性结论总是报告（它们释放 key，天然至多一次）。
2. **.NET 客户端只读消费面（本片随片落地，与 PLATFORM-1 重叠半边一次关闭）**：`IAgentConnection.SubmitResultAsync` ＋ `AgentConnection`（`SendAsync`→`Route.WorkSubmitResultRoute()`）＋ `ResumableSession`（`RunQueryAsync`：故障重连一次后重发，只读安全）＋ `FixtureAgentConnection`（只如实回答自己受理过的 id，其余 Unknown）。协议路由与 DTO（`WorkSubmitResultRequest/Response`、`SubmitPayloadDigest`）由 PLATFORM-1 线提供，本片未改。
3. **布局夹具**：`SubmitWorkAsync` 记录自己受理的 id；`SubmitResultAsync` 据实回答 `AlreadyAccepted`/`Unknown`——夹具不伪造账本事实。

## 回归（每项均先在旧 Goal 匹配逻辑上复现失败，再随修复转绿）

`WorkbenchBackpressureAndIdempotencyTests`（新增 `LostReceiptConnection`：回执必丢＋账本可编程，记录每次查询的 id/digest 并按正确服务端语义回显）：

1. `An_unknown_submit_is_resolved_by_the_exact_request_query_not_goal_matching`——未知提交必须以**相同 id＋`SubmitPayloadDigest.Compute(goal)`** 查询账本，Accepted 结论解除未知、报 task id、释放 key（旧代码从不查询：红）；
2. `A_same_goal_task_in_the_snapshot_does_not_prove_admission_while_the_ledger_cannot_testify`——快照出现同名目标任务且账本答 Unknown 时：key 保留、输出不得声称「解除」（旧代码在此清除 key 并宣布解除：红）；
3. `A_known_rejected_conflict_is_reported_and_the_dead_id_is_released`——KnownRejected 报冲突＋摘要证据，释放死 id（旧代码 key 永不释放：红）。

`RestoreWalkthroughTests`（脚本宿主 `AnsweringScript` 升级：新增 `submit_result` 线上回答＋submit 帧记录 client_request_id；两个走查随 F06 语义改写，由真实 `ResumableSession`＋线协议驱动）：

4. `Unknown_submit_is_resolved_by_the_ledger_receipt_without_any_replay`（原「同名任务可见即解除」走查反转为正确语义）——快照**不含**同名任务时，账本回执照样解除未知；全程零 submit 帧重发；解除后新目标正常提交（恰好 1 帧）；
5. `Unknown_submit_across_a_restart_stays_unknown_until_the_operator_resubmits`（原「保守分支文案」走查升级）——重启宿主 fresh 账本答 unknown：未知保持未知（输出无「解除」）、key 保留、零自动发送；操作员重发同目标时**同一 client_request_id** 恰好一帧入账并以回执收口。

既有 `Concurrent_snapshot_and_timeout_keep_the_outstanding_submit_key`（R07）不改语义，在新路径下保持绿（Pending 不查询、未知保 key、同 id 重试）。

## 实际检查

- `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`：**101/101 通过**（本切片前基线 95＋本片新增 3；计数含并行线同期落地的 3 项平台测试与 CORE-3 无关项）。
- `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`：0 警告 0 错误。
- 新增 3 项回归的先红后绿在本会话内实际执行（filter 单跑：改 ViewModel 前 3 失败/3 通过 → 改后 6/6）。

## 未验收（如实记录）

- 未提交、未推送、未跑远端 CI；真实宿主的 `submit_result` 端到端未在本机重跑（协议/宿主侧 e2e 属 PLATFORM-1 线的验收；本片以脚本宿主线协议走查覆盖消费面）。
- `work.submit_result` 的共享双语 fixture（Rust/.NET 同批 JSON）是否补齐由平台线按共享契约规则决定；本片未动共享 fixture。
- GUI-3 其余三条（追加指令绑定选定 task、取消按钮区分取消请求与可信停止、重连 watermark 接续的 UI 呈现）留待 C3 后续切片。
- 账本本身是 256 条进程内窗口、不承诺跨重启 exactly-once——本片 GUI 文案如实呈现该边界（Unknown/Expired 不冒充结论），不改变账本事实。
- 真实 provider 场景照旧 NOT_RUN。
