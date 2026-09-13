# GUI-4 补充回执：压缩成本入账（context_compacted 消费）

- 日期：2026-09-12（工作树，基线 `685b6bbb`，未提交）
- 归属：C4（GUI-4）成本面的收尾半；主体（model_used 三分账目＋context 新鲜度四态）见 [GUI4_COST_CONTEXT_DISPLAY_IMPLEMENTATION.md](GUI4_COST_CONTEXT_DISPLAY_IMPLEMENTATION.md)（并行 GUI 会话落地，本会话验证 114/114 后在空闲写入窗口接续）。M18 任务书「GUI 成本面由 C」；无 Rust/协议/客户端库改动。

## 用户能做什么

「这次维护花了多少」现在可见：每个 `context_compacted` 事件在运行日志入一行**服务端报告的压缩消耗**（原因、输入/输出 tokens、移出条数），状态条合计追加「压缩 N 次（输入 X · 输出 Y，服务端报告，未带实测身份）」。

## 诚实边界（本片的核心约束）

`ContextCompacted` 事件**不携带 usage_identity**（与 `model_used` 不同），所以压缩计数：

- 以「服务端报告，未带实测身份」呈现，**绝不升级为实测**；
- **不并入主调用实测合计**——实测桶只含 `usage_identity=observed` 的模型轮；
- 断开连接随账目一起重置（沿用既有 `ResetCostAccount` 清理点，不跨纪元携带）。

## 实现

- 字段：`_costCompactions/_costCompactionInput/_costCompactionOutput`；
- `HandleEvent` 新增 `context_compacted` 分支 → `RecordCompactionCost`（typed JSON 字段直读：reason/input_tokens/output_tokens/source_items，缺失按 0 并如实呈现）；
- 摘要行抽取为 `BuildCostSummaryText()`（原有三段措辞逐字保留，仅追加压缩段——既有测试断言不受影响）；
- `ResetCostAccount` 复位压缩计数。

## 回归（1 项新增）

`Compaction_events_show_service_reported_cost_outside_the_model_bill`：压缩事件后合计含「压缩 1 次」且实测仍为 0 轮（计数不漏进模型账单）；随后的 observed 模型轮与压缩段在摘要中共存不合并；日志行带「未带实测身份／不并入主调用实测合计」。

## 实际检查

- `dotnet test`：**115/115**（并行会话 114＋本片 1）。
- `dotnet build apps/Agent.Desktop`：0 警告 0 错误；`doc_consistency.py` OK。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；真实宿主 GUI 人工走查未做；真实 provider 的金额与缓存收益对比照旧 NOT_RUN（属 M18 COST 阶段验收）。
- 若 C 线（COST）后续给 `ContextCompacted` 补 `usage_identity` 字段，本面板按新事实升级呈现；当前不预设。
