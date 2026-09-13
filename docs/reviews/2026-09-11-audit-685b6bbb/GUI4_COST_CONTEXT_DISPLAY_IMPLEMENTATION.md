# GUI-4（C4）消费面：成本账目与上下文新鲜度展示——实施回执

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 切片：C4 GUI-4 的直接消费增量——把 PLATFORM-4 的 usage 身份与 PLATFORM-2 的 context 分型字段接进正式工作台
- 实现落点：`apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`、`apps/Agent.Desktop/MainWindow.axaml`、`clients/dotnet/Agent.Client.Tests/WorkbenchReviewTests.cs`

## 用户能做什么

1. **「这次取消有没有未知费用」**：状态条实时显示三分账目——`成本账目：实测 N 轮（输入 X · 输出 Y · 缓存读 Z）· 估算 M 轮 · 未知 K 轮`。每个 `model_used` 事件同时写一行运行日志：实测标「provider 上报」、估算标「运行时近似推导，非 provider 账单」、未知标「证据丢失（例如取消在飞调用）——按未知计，不计为零」。**三个桶永不合并**：unknown/estimated 轮绝不被折算进 observed 合计。
2. **「这条资料是新是旧、在不在最新表面」**：只读 Context 每行渲染新鲜度事实——`驻留 · 本轮已发送`／`驻留（未在最新表面）`／`暖缓冲`／`存储中（仅摘要指针）`；服务端缺字段（旧版本）诚实显示「新鲜度未知」，绝不从 kind 或正文推断。
3. 重试如实呈现：`含 N 次重试（失败尝试通常无用量，数值为下界）`。

## 实现

- `HandleEvent` 新增 `model_used` 分支，经 SDK 的 `RuntimeEventEnvelope.TryGetModelUsage`（PLATFORM-4 已落）读类型化事实，按 `usage_identity` 三分类累计并渲染；账目属**当前连接纪元**——断开清理（与 UI 缓冲、cancel 相位同一清理点）时重置，跨纪元不携带陈旧数字。
- `ContextItemRowViewModel` 增 `FreshnessLabel` 与 `Line` 后缀；`warm/cold` 归入「存储中」：两者都不是工作集，读者无 fetch 只见指针。
- AXAML 状态条增 `CostSummaryText` 绑定（与 watermark/focus/cancel 同条）。

## 回归（2 项新增，dotnet）

- `Model_used_events_classify_cost_by_identity_and_never_sum_unknown_as_zero`：observed/estimated/unknown 三事件依序入账——实测合计只含 observed 轮（`实测 1 轮`而非 `2 轮`）、估算与未知只计轮次、日志行带身份措辞。
- `Context_rows_render_freshness_from_typed_fields_and_admit_missing_as_unknown`：resident+sent／external／缺字段三行分别渲染「驻留 · 本轮已发送」「存储中（仅摘要指针）」「新鲜度未知」。

## 实际检查

- `dotnet test clients/dotnet/Agent.Client.Tests`：**114/114**（基线 112＋2）
- `dotnet build apps/Agent.Desktop`：0 警告 0 错误
- Rust 侧无改动（本轮只消费既有 wire 事实，无契约变更）

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；真实宿主上的 GUI 人工走查未做（数字正确性由 typed facts 单测保证）。
- 主 ViewModel 拆分按任务书「随功能逐步进行」，本轮未做纯重构。
- 压缩账目的展示（`ContextMaintenanceReport` 的压缩器分桶）随 `ContextMaintained` 事件已有结构化数据，本轮未接独立面板；真实 provider 的金额对比照旧 NOT_RUN。

## 下一步

至此三条线在文档队列中的全部非条件切片均已代码落地。剩余为统一用户旅程的端到端验收（需真实宿主＋真实 provider）与远端 CI 确认。
