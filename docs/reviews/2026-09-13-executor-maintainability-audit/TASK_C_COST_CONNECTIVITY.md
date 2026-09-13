# C：成本与接入——少做重复调用，如实记录全部已知消耗

唯一总队列：[NEXT_TASKS.md](../../NEXT_TASKS.md)。依据：[第三轮报告](REPORT.md)。当前源码为 `685b6bbb` 加共享未提交树；一次只交付一片，使用已有 Main/Maintenance 与 Unknown 事实，不重建费用数据库。

## 首片 COST-7 残余：失败尝试的已知用量不能降成未知

用户可以区分一次成功任务里主调用、压缩、失败与重试的已知消耗和未知部分。

修 R3-12：Chat/Responses 失败出口保留 accumulator 已收到的 usage；重试层保留每次尝试的已知消耗，未知仍明确 unknown。既有 EmptyCompactionSummary 透传只是一个分支，应复用小的失败用量表达，避免给每种错误各造一套成本转换代码。

C 拥有 provider 和 eval 汇总，B 单一合入必要 contracts/Runtime/DTO。不得在正文解析审批或完成状态，不把错误文本当权威用量，也不能将 canceled/unreported 写成 0。

必要回归：本地 SSE length/incomplete＋usage、参数格式失败＋usage→重试成功、无 usage 失败、Main/Maintenance、晚到结果去重。无需真实 provider 即可证明这些转换。

## COST-8 残余：退避绑定实际压缩输入

用户连续执行期间，相同的失败压缩请求不会因为未发送的历史尾部变化反复调用。

修 R3-13：C 定义“同请求”成本要求，A 单一维护 `context-baselines/rolling.rs`，提取小的只读 FoldPlan，让 digest、实际取料、partial coverage 与失败还源共用真实输入。无需新缓存服务或调度器。

必要回归：最旧记录超过 2000 字符、追加未进入 source 的记录，仍只调用一次；source/实际请求改变后可以重试；冷恢复维持退避。保留单 pass 的 count/token 上限及默认值。

## COST-9：成本累计独立于可丢弃渲染队列

用户界面短暂卡顿后，已收到的模型与压缩用量仍保留，费用不会因为日志渲染限额少算。

修 R3-14：在 GUI 丢弃呈现行之前更新固定大小累计值和 run/seq 水位；渲染只读投影。遇断流/重连无法补回的部分如实标不完整，禁止改成无限队列。复用当前费用分桶和事件身份，不在 GUI 自建业务完成判定。

必要回归：冻结 dispatcher，在一条 ModelUsed 后加入超过 64 条事件；Main/Maintenance 和 ContextCompacted 累计不丢、不重复；切换连接不会串账。

## 保留与统一成本验收

独立 maintenance transport、请求级输出 cap、cache miss/write 区分、零预算不挂压缩器、Unknown 计数均已有实现，不再列为缺失。非零 Dynamic 预算与跨恢复成本窗口是未完整核实的支持范围，不从静态字段存在推断保证。

真实同起点/同质量/同任务成功率的长期全成本对照继续 COST-5。先修已知浪费与账目丢失，再用明确环境和额度的有界真实窗口验证；本轮不调用付费模型。不能以本地稳定前缀或 cached token 比例代替账单节省，也不能冻结过期上下文、工具可用性或 GC 来提高命中。
