# 新测试结果应该怎样解释

基线bcacf41b；本文件只陈述本轮已读内容的证据范围。

## 证据矩阵

| 问题 | 已读证据 | 本轮可得结论 | 不能扩展成 |
|---|---|---|---|
| 是否有新分支实现 | Git commit/compare，38变更路径 | 新分支基于24c354cb增加一个提交 | 全部变更源码已逐行审完 |
| 新SHA的CI | GitHub Actions查询0；ci.yml仅main push/PR | 尚无该SHA Actions结论；非失败判定 | 借父SHA绿色 |
| 失败后能否恢复 | failure_resume测试源码；L1回执 | 有定向测试与真实output-limit后恢复报告 | 每个交错/20故障组合都验证 |
| 大工程是否纯自主完成 | FULL-PLAN、L1、L1-COMPLETE | 明确COMPLETE_WITH_MANUAL_REPAIR | 模型独立完成所有应用/文档 |
| 90分钟是什么 | FULL-PLAN回执 | 独立控制器拥有producer和四个应用worker，报告全量oracle通过 | Rust Runtime自身持续90分钟完成全部GC/取消/恢复边界 |
| FULL_PLAN_COMPLETE含义 | scenario与PLAN | 高层阶段/负载有回执，F12标verified，其余许多fault仍planned或existing_facility_to_verify | F01–F20均PASS |
| KV是否实际命中 | T8 Chat真实回执 | Endpoint接受与provider hit已有报告 | 当前Responses endurance是同一口径的配对降本基准 |
| 花了多少钱 | FULL-PLAN峰值估计、runner代码 | 报告估计约$1.04；账目提取与cap仍有缺口 | 已从全部raw SSE独立对账、证明绝不超预算 |

## 分开计算完成

新预算策略把最后一轮留作文本final。FULL-PLAN阶段P1/P2、P3、P4、P5、P6、P8均使用24轮，P7为12轮，总156轮。这个分布应结合`DecisionBudgetFinalization`事件解释，不能从所有`turn_completed`反推模型自然收敛或任务operator完成。报告仍为awaiting_operator_review，这个边界是诚实的。

建议分别记录：自然停止、预算文本收尾、失败、取消、operator接受；再记录应用oracle前后及manual repair数目。修复后的最终产物好，不等于先前自主产物已经好。

## 原始证据可达性

本轮通过GitHub读取Markdown回执与runner源码；尝试固定SHA的`target/runtime-endurance-v1/incremental-platform-20260919/full-campaign-receipt.json`，返回404。这个结论只针对仓库该路径，不是说用户本地不存在或没有运行。没有取得原始全campaign JSON、SSE、soak控制器源码和最终应用工作区，因此本轮不能独立重跑oracle或重算全部成本。

下一份最小证据包应含：source/binary/app版本，manual repair前后diff与验收，冻结tests/oracle/fixture哈希，有效profile与预算，F/C case→事件/回执索引，进程归属与起止，脱敏usage/attempt清单。不要包含API key、Authorization头或完整敏感环境。

## T8的两个具体口径

T8报告Chat三段：cold input33266/hit16512、warm input6155/hit2816、cross-run input31943/hit16896。所有数字只重述报告，不用它们推导当前账号金额或回归效果。

回执后文的“cross-run first-request hit=16896”与表格矛盾：16896是整段11轮总和，首轮hit=1536。应改标签。warm两轮与cold十一轮完成的工作不同，不可仅比较输入总量就宣称布局降本。耐久runner使用Responses，cache字段必须由其实际返回格式判定，不能照Chat提取规则默认补0。

官方缓存文档仅作为前缀/工具契约匹配机制参考，不作为deepseek兼容端点价格或字段接受证据。费用比较必须使用同任务质量、相同起点与有效profile，并同时包括缓存写入、读取、维护和重试；未知保留未知。

## 已执行与未执行

已执行：七项独立离线机制检查；其中Popen是实际临时进程，setup语义是实际临时文件，均主动清理。控制流模型本身不证明目标Rust交错已触发。

未执行：Rust/.NET仓库回归、Windows进程清理、真实终端、用户应用oracle、付费请求、真实SSE全账复算。审查没有修改或推送任何仓库文件。
