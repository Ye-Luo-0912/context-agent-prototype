# COST-3 prompt 侧（A 线合入窗）：稳定前缀诊断清理——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 C 线 COST-3 的 **prompt 侧**（所有权规则「prompt 侧由 A 合入」）；对应 [REPORT.md](REPORT.md) D03＋D05。provider 侧测量与 D04（packing 复用优化）归 C 线，本片未做
- 实现落点：`crates/agent-runtime/src/prompt.rs`（渲染层＋测试）、`crates/agent-runtime/src/actor/model.rs`（D05 由 COST 线自行完成，见下）

## 用户能做什么

相同证据的后续轮次，模型请求的前缀不再被每轮变化的目录计数破坏——provider 前缀复用的首个失配点从「working 消息头部」后移到「证据正文实际变化处」；计数诊断本身不丢失（ContextPrepared 事件与引擎 diagnostics 照常携带）。

## 缺口与修法

**D03**：`prompt.rs` working 消息头部的 `catalog total=N resident=… selected=…` 计数行几乎每轮变化，而 `ModelInput::into_request` 将整个 context_frame 作为复用候选——头部一行易变文本使前缀在**任何证据正文之前**失配。

**修（按任务书「可省的诊断留在事件或有界后缀」）**：计数行**整体移出模型请求**——它是可省诊断，事实保留在 ContextPrepared 事件与引擎 `diagnostics()`（两条既有观测通道），模型请求只携带证据正文与（CTX-4 的）必需上下文状态。语义边界：不排序重写证据、不冻结 Focus/GC/surface、不添加 filler。

**稳定性证明（离线诊断，非缓存收益声明）**：既有/修准的三项测试共同背书——
- `identical_items_assemble_identically_regardless_of_diagnostics_counts`（并行 COST 会话新增，本会话修复其随机 id 缺陷：`item()` 每次 `ContextItemId::new()` 使断言永假，改为固定 id）：相同 items＋不同计数 → 请求**逐字节一致**——即 items 不变时首个失配点不存在于该消息；
- `required_context_misses_are_rendered_with_recovery_paths`/`required_miss_rendering_is_bounded`（CTX-4）：必需事实仍渲染（稳定格式、有界行）；
- `selected_working_context_renders_catalog_census_and_path`（修准）：census 断言更新为「不得重新进入请求」＋证据 path 照常渲染。

**D05**：`actor/model.rs` 的无条件 `TEMP-DBG`/stderr dump 已由 COST 线自行移除（本会话核对时已不在树），无需再改。

## 实际检查（全部本地执行）

- `cargo test -p agent-runtime --lib prompt::`：**41 通过**（39＋2 项 CTX-4 计入；含并行会话 identical_items 测试修复后转绿）
- `cargo test -p agent-runtime --lib`：**401 通过**（共享树当前窗口全量）
- `cargo fmt`：本片文件干净；clippy 在本片文件无告警

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未调用真实 provider。
- **实际缓存收益未实测**（任务书原文：稳定字节只是诊断，不是缓存收益）——provider 侧首个变化段/复用边界/命中对照归 C 线 COST-5 窗口。
- D04（最终 packing 的 clone/序列化复用）未做：涉及 actor 核心 packing 路径，需 C 线测量确认瓶颈后再动；本片不改唯一真值结构。
- 共享树窗口内 COST 线在 compose/agent-runtime 的在途编辑与本片交错；`identical_items` 测试为本会话与其合流修复（随机 id 缺陷），语义归属并行会话的 D03 意图。

## 下一步

C 线 COST-3 provider 侧测量、COST-4（maintenance profile）；COST-5 阶段验收（真实 provider，条件项）。
