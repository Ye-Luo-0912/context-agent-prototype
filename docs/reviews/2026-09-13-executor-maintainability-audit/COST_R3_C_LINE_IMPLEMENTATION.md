# 第三轮 C 线实施回执：COST-7 残余 / COST-8 残余 / COST-9（R3-12/13/14）

日期：2026-09-13。基线 `685b6bbb` 加共享未提交树；依据 [第三轮报告](REPORT.md) 与 [C 任务书](TASK_C_COST_CONNECTIVITY.md)，一次一片顺序交付。

## COST-7 残余（R3-12）：失败尝试的已知用量不再降成未知

- **契约**：新 `AgentError::FailedWithUsage { usage, source }`——唯一共享的失败用量表达，包装原错误（`source` 保留类别与可重试性）；构造器 `failed_with_usage(usage, source)` 在 usage 无任何已报告计数时原样返回原错误（空信封永不当证据）；`reported_usage()` 同时识别它与 `EmptyCompactionSummary`；`failure_source()` 供分类看穿；`ModelUsage::has_any_reported()`。
- **provider**：Chat 尾部（length / terminal error / 缺 [DONE]）与 Responses 尾部（failed/incomplete 的三类终态 / 缺 completed / finalize 参数错误）全部**先读 accumulator 的 usage 再失败**，有报告即包装；`ResponsesAccumulator::usage()` 新访问器。
- **重试层**：三条重试路径（complete / stream live / stream buffered）记录最近一次失败尝试的已知 usage；give-up 时若最终错误自身无 usage 则以 `FailedWithUsage` 携带（原始错误作 source，分类不变）；`retry_class`/运行时分类经 `failure_source()` 看穿——包装的 Transport 失败仍可重试。
- **Runtime**：`OperationOutcome::Failed` 增 `usage: Option<ModelUsage>`（serde default，B 单一合入契约）；模型 op 转换点从错误提取；actor Failed 分支改为——有已报告 usage 发**真实计数行**（身份随报告、Main lane、attempts≥1），无证据保持显式 Unknown 行；两种路径都登记去重队列（与取消/晚到机制一致，一笔一次）。
- **回归**：provider 流级——length＋usage 保留计数且 OutputLimit 类别可读、缺 DONE＋usage 保留（部分计数缺席即缺席）、无 usage 失败保持原错误、Responses failed＋usage 保留；retry 层——give-up 保留更早尝试的已报 usage 且原类别可读、usage 包装的 Transport 失败仍重试成功；runtime——失败带 usage 发 Observed 真实行（321/78、Main），无 usage 保持 Unknown。**provider 134**（+5）、retry 定向 42、runtime lib **407**、turn stream 7（+1）。

## COST-8 残余（R3-13）：退避绑定实际压缩输入

- **共享装箱计划**：新只读 `plan_fold_packing(state, config) -> FoldPacking { consumed, partial }`——精确描述**实际会进入本次 2000 字符输入**的整取数与切分前缀；`take_fold_job` 的取料与 `fold_request_digest` 的退避摘要改用**同一份计划**（摘要＝prior 摘要 id＋实际整取记录 id＋切分记录 id 与前缀长度）。追加进不了 source 的候选尾部不再解除退避；实际输入变化（新记录能进 source）仍立即重试。失败还源、partial suffix 身份、单 pass count/token 上限与默认值全部保留（W04/COST-4 语义不变）。
- **回归**：`an_unsent_tail_append_does_not_retrigger_the_failed_fold`——最旧记录以切分前缀填满输入失败后，追加新候选（旧代码改变候选集摘要→第二次调用）现在仍只调用一次、延期可见；`a_real_source_change_still_allows_the_retry`——新记录能进 source 时退避解除立即重试（恰两次调用）；既有同请求退避、内容变化重试、冷恢复维持退避回归全部保持。**baselines 25/25**。
- 所有权说明：按队列「C 第二片，A 单一改 Rolling」，本片为 C 提出成本判定（同请求＝实际 source＋相关请求字段）后在共享树落地的 A 域文件修改，回执在此登记。

## COST-9（R3-14）：成本累计独立于可丢弃渲染队列

- **修复**：GUI 固定大小累计字段成为费用事实的唯一权威，日志行为有损投影。渲染队列（64 条）丢弃最旧行**之前**，先从该行提取 model_used/context_compacted 事实并入账（`AccumulateCostFactFromShedRow` → 与活渲染路径共用同一 `AccumulateModelUsage`/`AccumulateCompactionCost`，恰一次）；累计变化经 `_ui.Post` 刷新摘要投影；新增 `_costRenderRowsShed` 计数（随连接纪元重置）提供丢弃可观测性。未移除渲染 cap、未建无界队列、未自建完成判定；主/维护/压缩分桶与事件身份全部复用。
- **回归**：`Cost_facts_survive_the_render_queue_cap`——1 条 model_used 后跟 69 条填充事件把它挤出队列（冻结 InlineUiDispatcher），汇总仍显示实测 1 轮（9000/120）且恰一次；`Compaction_cost_survives_the_render_queue_cap`——被挤出的压缩行仍入压缩合计（34600 输入）且不重复。旧代码上被挤出行直接丢失，两测必红。**dotnet 123/123**。

## 验证汇总（实际命令）

- `cargo test -p provider-openai`：**134/134**；`cargo test -p context-baselines --lib`：**25/25**；`cargo test -p agent-contracts --lib`：**174/174**；`cargo test -p agent-runtime --lib`：**407/407**；`cargo test -p agent-runtime --test turn -- stream::`：7/7；`dotnet test`（Agent.Client.Tests 全量）：**123/123**。
- `cargo fmt --check`（四 crate）通过；contracts/provider/baselines clippy **0** 警告。
- 共享树并行域（如实记录）：A/B 线第三轮任务（CTX-10/EXEC-9 等）在飞，验证窗口间偶见非本片文件的编译中间态，本片全部验证在可编译窗口完成；runtime 转换点的 `usage: Default::default()` 初版适配由并行会话落下的本片契约字段，已由本片替换为真实提取。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI。
- 成功路径上更早失败尝试的已知计数仍无逐尝试事件通道（当前行＝最后成功尝试的报告＋retries 下界标记）；如需逐次账目需新的契约面，记入残余。
- COST-5 真实执行继续 PREPARED / NOT_RUN（阻塞与入口见 [准备回执](COST5_PAIRED_ACCEPTANCE_PREPARATION.md)）。
