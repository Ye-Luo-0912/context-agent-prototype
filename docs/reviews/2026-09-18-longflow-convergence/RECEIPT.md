# 长流程根因确认与有界选择算法修复

用户授权：复查频繁的反复检查、不交付问题，确认根因并优化通用算法。基线 `24c354cb` 加本任务已有的未提交证据覆盖/提示生命周期修复；这些改动保留，未重置共享工作树。本回执是本地验证，不代表提交、远端 CI 或任意模型质量保证。

## 根因与证据强度

1. **确定性正文选择缺陷。** 上一版只把候选数量从 4 改为 16，但 `file_read_exposure_windows` 仍按时间正序在第 16 次读取停止。后续新代码永远到不了选择器；四个输出槽也偏向最老材料。三个 Rust 反例先执行为红：40 次旧读取后当前新文件无法恢复；八个候选选中最老的四个；完整 10KB 正文一份也无法恢复。红日志：本地 `target/longflow-algorithm-20260918/red.log`。
2. **确定性预算不匹配。** 成功工具结果允许约 16K 字符，而正文恢复逐项限制 8KiB。StockLedger 的两个正常代码窗口分别为 8963 和 8655 字节，进入过模型但一旦离开六交换尾部便整块被恢复层拒绝。版本已知不能替代正文可见。
3. **确定性预算交付缺口。** Actor 有有限轮数，原本每个决策都能继续发工具，预算外才失败，因而最后一批工具结果没有用于答复的决策。先投影剩余预算后，真实模型仍能在最后一轮继续发工具，说明软提示不能保证答复。此问题与“是否已完成任务”是不同事实。
4. **模型语义与验证覆盖问题。** 真实请求一直携带 README 要求，停滞提示也在旧实验第二段 R9–R18 持续存在；模型自行观察到 CSV 被错误接受后仍继续探查。不能把模型未据观察行动全部解释为 Runtime 丢失了指令。生成代码通过公开测试也没有证明全部语义，独立反例仍必需。

格式错误重试差异的代码边界也已明确：`LiveSink::creates_replay_barrier` 将已广播的 TextDelta 视为不可回放，`RetryingTransport::complete_stream_live` 因而不再重试；只有内部 tool delta 的失败仍能有限重试。旧实验首段没有保存原始 SSE，不能断言那一次一定由此分支触发。本片保留该副作用/显示边界，不把输出重复包装成安全重试。

## 实现

- `prompt.rs`：选择器从最新工具结果开始流式扫描当前 TurnFrame。先判断结果类型、Fresh path@revision、保留尾/已选窗口覆盖、单项和剩余总字节，再入槽。拒绝、重复候选不占槽；诊断用的最多 16 个 demand 样本不再截断实际选择扫描。只有入选正文才复制。源扫描受已有回合帧约束，选择状态最多四项，比较使用已有窗口覆盖语义，不引入第二个调度器。
- `execution/body_cache.rs`：每项上限 16KiB，缓存和回注共用 32KiB 总预算，仍最多四项；LRU 同时按项数和总字节淘汰。保持完整窗口，不截正文后谎称全覆盖。预算总量没有从原四项×8KiB上涨。
- `TaskProgressView::decision_budget`：Actor 每次 BeforeModel 从自身 round/max 派生，序列化缺省兼容；执行状态持久化不保存新的倒计时权威。预算行参与同一计价与最终打包，保证初始预算/材料化/最终请求一致。
- **最后一轮交付**：N>1 时，最后一轮在原 N 次模型预算内沿 `RoundSurfacePlan` 切换纯文本，审计原因 `DecisionBudgetFinalization`；N=1 的单决策探针行为保留。无额外调用、无不确定工具重放、无 `task.complete`、无自动验收。正常模型可以说明结果或剩余工作；若仍伪造工具调用，已有 surface enforcement 和硬轮数上限继续生效。取消、供应商错误和恢复围栏仍可能提前结束。
- 默认编码策略补充“观察→针对性修复/验证→交付”的一般约定，明确测试通过不等于全部交付、ordinary final 不等于持久任务完成。这是行为指引，独立于上面的确定性选择与预算机制。

本片的完整通用回归还包括：20 个过大新候选后仍选择可用旧正文、固定总预算内三份 10KB 正文、旧版本/截断正文/区间覆盖既有负对照、预算在进度块裁剪后仍可见、实际 Actor 请求逐轮预算递减、最后纯文本决策不额外调用/执行工具/完成持久任务。

## 真实模型验证（非配对因果实验）

复用原 StockLedger 的 TASK/public tests/frozen reviewer，重新从空骨架开始，DeepSeek Flash / Responses / Dynamic，单次输出 8192，首段和恢复段各 18 轮。证据目录：本地 `target/long-ledger-optimized-20260918`；请求正文原样转发，凭据不落盘。

| 段 | 模型轮 | 工具调用 | 结果 |
| --- | ---: | ---: | --- |
| 1：正文算法＋软预算 | 18 | 30 | 公开测试通过；budget stop，未交付 |
| 2：恢复原任务，无反例反馈 | 18 | 29 | README＋最终答复出现，TurnCompleted；公开20、独立12通过；补充3项中2项失败 |
| 3：两条明确反例＋文档纠正反馈 | 10 | 18 | 两个实际行为缺陷修复；但预算停止，文档仍有错误、自测未全部通过 |
| 4：最终加入纯文本预算机制后恢复 | 4 | 6 | 第4个真实请求 tools=[]；审计为 decision_budget_finalization；最终答复列出未完成项，TurnCompleted，task_completed=false |

前 3 段和第 4 段是不同二进制，SHA 和源码 diff 分别在各段 metadata/runtime.patch，不能把前面三段当作最后机制已启用。四段合计 50 轮、83 次工具、148.89s；input 710957、output 30169、cached input 201088、50 attempts、0 retries。成本金额未测量，不能声称费用下降。模型随机性、生成代码和后续反馈不同，不把上一轮 50 轮与本轮第 36 轮首次答复冒充单变量收益。

**最终应用验收边界**：公开20、冻结独立12、补充3全部通过；Agent 自测共63项，61通过、2错误。两个残余分别是：模拟 csv.reader 构造阶段异常未被包装；自测擅自要求 Ledger(directory) 抛 ValueError（原要求只约束 CLI，此测试超出了原异常类型契约）。README 仍错误地声称 dict 字段顺序差异会改变 canonical 身份。外层没有代改生成应用来美化结果。因此这次证据证明正文和交付机制修复，**不证明任意模型能完整满足复杂任务语义，也不把该应用标为全部验收通过**。

## 本地验证

- 修复前的三个正文选择反例：3 failed；修复后通过。
- `cargo test -p agent-runtime --lib --test actor --test turn`：436＋101＋146 passed。
- 初次完整 Runtime 回归发现一条旧断言假设首次请求无 TaskProgress；已改为核对真实预算，保留2048字符上限和不虚构结算的检查，最终受影响 turn target 全绿。
- `cargo check --workspace --all-targets`、Runtime/contracts clippy `-D warnings`、fmt、doc_consistency：通过。
- `cargo test -p agent-contracts -p agent-tui -p agent-compose --tests`：376 passed、7 ignored、0 failed（其中 Contracts 198、TUI 106＋2、Compose 各 target 合计70）。与最终 Runtime 三个 target 合计1059 passed。消费者日志为 `target/longflow-algorithm-20260918/consumer-tests.log`；不借用旧 CI。

没有添加 stdout 成功文本解析授权、按 argv 跳过任意进程、强制 TaskCompleted 或新的持久任务表。尚未提交/推送，Unix 与远端 CI 未执行。
