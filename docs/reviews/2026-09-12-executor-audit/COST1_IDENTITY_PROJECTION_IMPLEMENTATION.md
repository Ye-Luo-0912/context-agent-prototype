# COST-1 首半回执：压缩身份贯通事件投影＋账目分类（E05.3）

- 日期：2026-09-12（工作树，基线 `685b6bbb` 加既有未提交修改，未提交）
- 归属：M18 **C 线 COST-1（完整调用账本）** 的「压缩身份在事件投影丢失」半（E05 断点 3）；[派发文档](HANDOFF_COST.md) 要求的「最小 usage/compaction 事件增量与兼容 fixture」即本片交付。GUI 成本消费面此前已落（C4 两回执）。
- 所有权执行：contracts 事件增量按派发文档属「交 B 合入」；当时 B 在途文件为 actor/*（EXEC-1），`event.rs` 已 19 小时无写入，按既有「重叠切片一次执行、双边关闭」惯例垂直合入；fixture 为共享契约要求的兼容金样。

## 修了什么（E05 断点 3）

`ContextCompaction` 自带 `usage_identity`（typed），但 `RuntimeEvent::ContextCompacted` 投影丢弃它——压缩成本因此无法按证据分类，eval「仅 observed 汇总」对压缩链路不成立。现：

1. **contracts**：`RuntimeEvent::ContextCompacted` 增 `usage_identity: UsageIdentity`（`#[serde(default = "unknown_usage_identity")]`——旧事件行解码为 Unknown，零计数不得读作已观测消耗）；发射助手 `context_maintenance_events` 从报告逐条透传。
2. **agent-eval metrics**：`ContextCompacted` 分类入账——仅 `Observed` 进入 `compaction_input/output_tokens` 合计；`Estimated`/`Unknown` 以新增的 `compaction_estimated_events`/`compaction_unknown_events` 计次，**不折入 token 合计（未知不充当已消费零）**。`ContextMaintained` 兜底分支未动（无事件时的旧聚合路径，改动留待 COST 后续与 A 线对齐报表形状）。
3. **共享 fixture**：新增 `event_context_compacted.json`（notification 帧，`usage_identity:"estimated"`），Rust `event_fixture_pins_compaction_identity` 与 .NET `Event_fixture_pins_compaction_usage_identity` 双侧逐字节 roundtrip＋类型化断言（含 `EventType`/`IsLiveOnlyProgress=false`）。
4. **GUI**：压缩成本行按 wire 身份分类——`observed`→实测、`estimated`→估算（运行时近似，非 provider 账单）、`unknown`→usage 未知（按未知计，不计为零）、缺字段（旧事件）→「服务端报告，未带实测身份」；任何身份都**不并入主调用实测合计**。GUI 测试：带身份事件按估算措辞渲染，旧事件保持诚实措辞，实测账单不受污染。

## 兼容性

- wire 只增字段且带 serde default：旧事件 JSON 解码为 `Unknown`（测试 `legacy_compacted_rows_decode_as_unknown_identity` 钉住）；新 JSON 双语言 roundtrip 逐字节一致。
- `bundle.rs` 的成本 JSON 为字段白名单，新增 metrics 字段不进入既有冻结产物输出。

## 实际检查

- `cargo test -p agent-contracts --lib` **173**；`cargo test -p agent-platform-protocol` **47＋13**（含新 fixture 测试）；`cargo test -p agent-eval --bins` **223**（含新增 2 测：分类计数、旧行解码 Unknown）；`cargo check -p agent-tui`（解构更新）通过；`cargo fmt`（涉及 crate）通过。
- `dotnet test` **117/117**（115＋fixture＋GUI 身份测试）；`dotnet build apps/Agent.Desktop` 0 警告 0 错误；`doc_consistency.py` OK。
- 如实记录：`cargo test -p agent-runtime --test turn` 有 1 项 safepoint 失败——该文件属并行 EXEC-1 会话分钟级在途修改（`actor/tools.rs` 等正是 E05.1 的点名文件），非本片改动面（本片不触碰 agent-runtime），归属 EXEC-1 收口。

## 收尾半（2026-09-12 同日补齐）：E05.1 ＋ E05.2

EXEC-1 落地、actor 文件空闲后，按「重叠切片一次执行」继续收口 COST-1 其余两断点：

**E05.1（普通模型失败不记账）**：`OperationOutcome::Failed` 终态此前只发 `Failure` 即结算——失败/中断的模型轮从账本消失。现 `completion.kind == OpKind::Model` 时经新助手 `emit_unknown_model_usage_row`（与取消路径共享实现，pub(super)）发一条 `usage_identity=Unknown` 的 `ModelUsed` 行；`ModelUsage::default()` 与零计数按 Unknown 语义呈现（未知不充当零）。失败结果今日不携带部分用量——当 transport 未来浮出部分计数时必须按其诚实身份保留（代码注释钉住该要求）。

**E05.2（rolling 压缩失败不记账）**：`rolling.rs` 维护循环两处收口——① `compact_fold` 失败（压缩器调用 Err）时推入一条 `usage_identity=Unknown` 的 `ContextCompaction`（零计数＋`source_items`），保源回退、FoldRestore 守卫语义逐字不变；② 移除「token 非零才入账」的门槛——成功但零用量的调用按其自身身份照常入账（provider 未报 usage 不再被静默丢弃）。无压缩器的引擎（fallback pass）不产生条目（无调用即无成本）。两条路径经 `context_maintenance_events` 自动获得首半的事件身份透传与 GUI/eval 分类。

**回归（新增 3 项）**：runtime `a_failed_model_round_leaves_an_unknown_usage_row`（自发失败的模型轮终态带 unknown 行＋Failure 事件）；baselines `failed_compaction_preserves_the_pre_fold_state` 扩展（失败报告必须含 Unknown 压缩行）＋新增 `zero_usage_compaction_is_still_accounted_with_its_identity`。

**实际检查（收尾半）**：agent-runtime actor **86**、turn effects **16**、contracts **173**、context-baselines **18**（17＋1）、agent-compose 全部测试目标 0 失败、clippy（runtime/baselines/eval/contracts）0 警告、fmt 通过。

**如实记录的边界**：模型轮的「stale 完成被丢弃」路径（轮已被替换）其用量仍不入账——超出 E05.1 点名范围，记为残余（成本真实存在，消费端可按 Unknown 计次承接）；safepoint 1 项失败经本会话 `EXEC1_RED_CHECK=1` 独立复核 EXEC-1 归因成立（reverted 仍败）且 safepoint 实现/测试文件零未提交差异——共享树跨线缺陷，归集成者/B 线收口，非本片引入。

## COST-1 剩余（交接记录）

- **E05.4（供应商字段：DeepSeek hit/miss、Responses cache-write、压缩输出丢 attempts/retries/缓存计数）**：归 COST-2（provider 映射），本片未动。
- E05.1/E05.2/E05.3 落地后，GUI 与 eval 已就绪按身份分类，E05.4 落地后无需再改消费端。
