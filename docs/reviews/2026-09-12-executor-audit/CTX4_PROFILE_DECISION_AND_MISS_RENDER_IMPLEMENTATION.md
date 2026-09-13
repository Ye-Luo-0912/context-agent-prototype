# CTX-4 默认 profile 决策与模型可读 required-miss 呈现——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 A 线收官片 CTX-4；依赖 CTX-1/2/3、EXEC-1（并行已收口）
- 实现落点：`crates/agent-runtime/src/prompt.rs`（required-miss 模型可读渲染）、`crates/agent-compose/src/lib.rs`（profile 决策声明）、`crates/agent-runtime/src/prompt.rs` tests（2 项新回归）；无 wire/契约变更

## 用户能做什么

1. 默认入口的行为与声明一致：宿主未指定 context policy 时保持 **Rolling**，其必需正文义务由 baseline 的 `required_claim_misses`（CORE-2：每个 PromptRequired 如实报 Missing）承担——「不支持必需正文投影」永远不会被伪装成「全部已满足」。
2. **必需依据缺失对模型可操作**：引擎上报的 required miss 现在渲染进最终模型请求——`REQUIRED CONTEXT STATUS (not satisfied)` 块，逐行给出原因类别（Missing/Corrupt/IoFailed/BudgetExcluded/PolicyExcluded）、requirement 身份（item_ref＋source）、并以产品真实恢复入口收尾（`context.search` / `context.fetch` / `artifact.read`，不可恢复则如实告知操作员）。沉默即默认满足的旧风险闭合。

## Profile 决策（任务书第一项交付）

**决策：生产默认保持 `ContextPolicy::Rolling`＋最低义务呈现，不切 Dynamic。** 理由：CORE-2 已让 baseline 引擎如实上报必需正文义务（ truthful unsupported 信号），CTX-4 又让该状态模型可见、可操作——Rolling 的最低正确性边界已闭合；切换 Dynamic 是质量/产品决策，必须先通过本片任务书的真实默认入口旅程验收，不因计划完成而默认改写。实验 baseline（append/rolling）可比语义不变。声明落点：`agent-compose::build_context_engine` 文档（决策、理由、切换条件）。

## 渲染语义

- 有界：最多 8 行 miss 行；超出部分与引擎侧 omitted 合计为一行 `... N further misses omitted (bounded render)`。
- 事实型：逐行 reason 类别＋item_ref＋source_field_id；不渲染恢复指令之外的操作建议，不伪造 settled 状态。
- 恢复入口：`context.search` / `context.fetch`（按 item_ref）、`artifact.read`（按来源）、不可恢复时明确让出操作员——均为产品既有入口，无新增工具。

## 回归（2 项新增，红-first）

| 测试 | 覆盖 |
|---|---|
| `required_context_misses_are_rendered_with_recovery_paths` | miss 进最终请求：状态块、item_ref、原因类别、恢复入口（无渲染时红） |
| `required_miss_rendering_is_bounded` | 40 条 miss → 渲染行有界＋omitted 计数可见 |

## 实际检查（任务书定义的定向范围）

- `cargo test -p agent-runtime --lib prompt::`：**40 通过**（38＋2）
- `cargo test -p agent-compose --lib`：**32 通过**（决策声明所在 crate 全绿）
- `cargo fmt --all -- --check`：本片文件干净（剩余 diff 在 kv_cache_walk/doctor 等并行在途文件）

## 未验收（如实记录——CTX-4 的验收半）

- **真实默认入口旅程验收 NOT_RUN**：任务书要求的「新增约束→跨文件编辑→预算让出→继续→冷恢复」全旅程需真实宿主＋真实模型/provider（条件项）。本片交付决策＋呈现＋回归；旅程实测归集成者按 COST-5/统一旅程窗口执行，届时录下实际 profile、最终请求、required misses 与恢复结果——**在此之前不得宣称 Dynamic 优于 Rolling 或宣布默认能力验收通过**。
- 未提交/推送、未跑远端 CI。
- 共享树事实：COST 线在 compose（COST-4 maintenance profile）与 EXEC-3 在 workspace lineage 的编辑中间态窗口与本片验证交错，均已轮询至收敛后复跑；并行中间态的失败/告警不归属本片。

## 下一步

A 线（CTX-1..4）全部代码落地。剩余队列：EXEC-4（B 线，读入有界）、COST-3/4/5（C 线，按依赖推进）——归各自线执行者。
