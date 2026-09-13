# CORE-4 首切片：模型调用记账真值（observed / estimated / unknown 身份）——实施回执

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 切片：CORE-4「缓存与压缩的实际成本优化」的记账真值半；对应任务书「主请求、压缩、重试、取消统一记账，保留每次 usage 的 observed / estimated / unknown 身份。未知不是零，cache miss 不是 cache-write 计费证据」
- 实现落点：`crates/agent-contracts/src/{model,event,compaction,context}.rs`、`crates/agent-runtime/src/actor/{tools,turn,status}.rs`、`crates/agent-compose/src/compactor.rs`、`crates/context-simple/src/engine.rs`、`crates/context-baselines/src/{lib,rolling}.rs`、`crates/agent-eval/src/{metrics,m15_report,fixture_driver}.rs`；不改 GUI、不改平台路由、不动 CurrentStateLast 布局与缓存边界

## 用户能做什么

长任务的每一类模型花费（主请求、压缩、重试、被取消的回合）在账目上都能回答「这个数字是从哪来的」：provider 真实上报（observed）、运行时近似推导（estimated）、还是证据丢失（unknown）。**未知不再冒充零**——丢 ACK 的取消、失败前的压缩调用都留下显式的 unknown 行，成本汇总不会把没见到的账单算成没花钱。

## 缺口与修法

基线三处违规（均为源码核实，非推测）：

1. **`ModelUsed` 事件拍平 unknown→零**：provider 层 `ModelUsage` 本就用 `Option<u64>` 区分「未上报」，但事件发射处 `unwrap_or(0)` 把它拍平；`cached_input_tokens` 同样无法区分「没有缓存命中」与「provider 没报缓存明细」——正是「cache miss 不是 cache-write/无缓存证据」要防的误读。
   **修**：`UsageIdentity`（`observed`/`estimated`/`unknown`，serde snake_case）入契约；`ModelUsed` 增两个 serde-default 字段——`usage_identity`（旧行默认 `unknown`：拍平前的旧记录本就无法区分，诚实保守）与 `usage: Option<ModelUsage>`（精确类型化上报，逐字段 Option，含缓存计数）。旧 u64 字段保持下界语义不变。`.NET 客户端不消费 `model_used` 事件（已核实），无跨语言 wire 影响；共享契约变更由核心线按本阶段规则确认。
2. **取消在飞模型回合时 usage 静默消失**：取消先撕回合再发 `TurnCancelled`，在飞回合的 provider 花费（中断的服务端调用可能照样计费）完全不入账——旅程第 7 步「丢失 usage 的取消不能显示成零消耗」的正中反例。
   **修**：两条取消路径在 `TurnCancelled` 落审计前，若 `cleanup_kind == OpKind::Model`，显式发一条 `usage_identity=unknown` 的 `ModelUsed` 行（数值为零但身份标明 unknown，消费端不得计入观测消耗）。
3. **压缩器估算冒充观测**：`ModelBackedCompactor` 在 provider 未报 usage 时用 `approx_tokens` 静默填充，`CompactionOutput`/`ContextCompaction` 的 u64 无法区分数字来源。
   **修**：两类型增 `usage_identity`；`ModelBackedCompactor` 有完整上报→`observed`，否则→`estimated`（近似数值保留但身份如实）；context-simple 蒸馏失败分支（调用失败、花费不可知）→`unknown`；无压缩器/脚本化 fixture（没有模型调用、零是事实）→`observed`。两处 `ContextCompaction` 构造点从 `CompactionOutput` 透传。旧行 serde default `unknown`。
   评估侧 `metrics.rs` 聚合同步按身份分类：仅 `observed` 行计入 model_input/output/cached 汇总，`estimated`/`unknown` 行置 `provider_tokens_lower_bound`（重试既有语义复用）。

**明确不做（按任务书与边界）**：CurrentStateLast 布局、缓存边界、压缩器预算/延期统计（W04/W08 已有）全部不动；不新建 benchmark；端到端成本对比需真实 provider，照旧 `NOT_RUN`。`agent-eval/driver.rs`（冻结评测代码）只做编译适配（匹配带 `..`，行为不变），其汇总未按身份重分类——如实记录，不为普通改动重开 M15/LT-EVAL。

## 回归（5 项新增；前两项先在回退修复的代码上复现失败再转绿）

| 测试 | 覆盖 |
|---|---|
| `agent-runtime tests/turn/effects.rs::cancelling_an_in_flight_model_round_leaves_an_unknown_usage_row` | 挂起的模型回合被取消 → 显式 unknown 身份的 `ModelUsed` 行＋`TurnCancelled`（摘掉修复红、恢复后绿） |
| `agent-compose compactor.rs::a_missing_usage_report_is_labelled_estimated_not_observed` | provider 无上报 → 近似值＋`estimated` 身份；既有 `bounds_output_and_records_usage` 补 `observed` 断言 |
| `agent-contracts model.rs::usage_identity_follows_the_provider_report` | 完整上报→observed；单侧缺失→unknown（账单不完整≠完全观测）；全缺→unknown |
| `agent-contracts model.rs::legacy_model_used_events_default_to_unknown_identity` | 旧 JSON（无身份字段）解码默认 `unknown`＋`usage=None`，旧数值原样保留 |
| 既有 status/stream/ingest_cancel/m15 fixture 构造点机械补齐（显式身份），语义不变 |

## 实际检查（全部本地执行）

- `cargo test -p agent-contracts --lib`：**173 通过**（基线 171＋2）
- `cargo test -p agent-runtime --lib`：**391 通过**（基线不变；含新取消回归所在的 turn 套件 132）
- `cargo test -p agent-compose`：lib 32＋全部集成测试共 **50 通过**（含 compactor 5）
- `cargo test -p context-simple --lib`：**320 通过**；`context-baselines --lib`：**17 通过**（基线不变）
- `cargo test -p agent-core --lib`：**152 通过**；`tool-runtime --lib`：**261 通过**；`agent-workspace --lib`：**108 通过**（前序切片基线不变）
- `cargo test -p agent-eval`：**221 通过**（含按身份分类后的 metrics 断言）
- `cargo check --workspace --all-targets`：通过 0 警告（并行平台线在途文件恢复后于主树复核）；`cargo fmt --all -- --check` 通过；`cargo clippy -p agent-contracts -p agent-compose -p agent-eval --all-targets` 0 警告
- `python scripts/doc_consistency.py`：OK（13 live docs）

## 未验收（如实记录）

- 未提交/未推送、未跑远端 CI；未调用真实 provider。
- CORE-4 其余增量不在本切片：稳定前缀的进一步优化（须先有真实 wire 归因）、压缩专用 provider profile（须先追踪真实 wire 参数）、同起点任务的成本对比验收（须真实 provider）、GUI-4 成本展示消费端（GUI 线范围）。
- `agent-eval/driver.rs` 旧汇总路径未按身份重分类（冻结评测代码，只做编译适配）。

## 下一步

核心线 A1–A4 全部切片代码落地；本阶段核心线剩余为统一用户旅程的对接验收（依赖平台/GUI 二波）与条件项（真实 provider 照旧 NOT_RUN）。
