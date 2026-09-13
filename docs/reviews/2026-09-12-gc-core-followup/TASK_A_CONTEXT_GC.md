# A 份：上下文、GC 与长期证据

目标：Agent 在长期任务、外存故障和多次回收后仍拿到正确依据；工作集、GC 单次工作和历史元数据都受控，不用失忆换内存，也不用扩大上下文换缓存命中。

**本次交给执行者先做 CTX-5，一个切片交付后停止。** 后续顺序 CTX-6 → CTX-7 → CTX-8 → CTX-9。问题依据见 [REPORT.md](REPORT.md) 的 R2-03～07；总队列以 `docs/NEXT_TASKS.md` 顶部为准。

开工核对 `git status --short`、HEAD 和本目录 SOURCE_MANIFEST。当前有大量他人未提交代码，禁止恢复成裸 HEAD。前序 CTX-1/2/3 原反例修复定向确认后跳过；本轮补跨路径一致性。共享 `agent-contracts`、Actor/command/compose 接缝交 B 合入，A 负责领域语义。不要抢改 C 的 compactor 或 B 的恢复实现。

## CTX-5 / P1：Live 必需正文的保护贯穿维护和 GC

**用户结果：** 被当前任务明确持有的 live 证据不会被普通 TTL/老化提前终结；已终结的旧证据仍不可复活。

- 入口：`context-simple/src/residency.rs`、`gc/minor.rs`、`gc/full/mod.rs`、anchor 投影/required planner；契约为 PromptRequired/ResidentRequired/StorageRequired 的现有区别。
- 先复现 ResidentRequired Working item 跨 `ttl*4` 被 full sweep 逐出，以及 PromptRequired 非最新 Ephemeral item 跨 TTL 被 minor 终结；必须从 claim→maintain→gc→materialize 走，不能只测试 mark_roots。
- 在唯一的 live 保护判定中使用当前 anchor revision，Resident/Warm/Stored 保持一致。明确 root 更新失败与释放行为；旧/空投影不能冒充当前根。已经有明确 Superseded/VerifiedFixed/Tombstoned 事实的项保持终态。
- 必要回归：各强度、跨 TTL、切任务、撤销 claim、旧 revision、预算排除、死条目、存储读取失败、GC 前后真实最终请求一致。StorageRequired 只保护储存，不无条件把全部内容塞入 prompt。
- 定向验证：context-simple root/required/lifecycle 相关 target；需要改变投影时跑 runtime 对应上下文回归。不调整冻结 GC 分数/阈值。
- 停止条件：所有合法 live 持有义务被满足或明确报告不能满足，且终态不会因保护复活。

## CTX-6 / P1：Pending 是完整 owner，不是语义旁路

**用户结果：** 外置写入失败时，用户仍能撤销旧决策、消除已经验证修复的错误、取回必需正文。

- 入口：`engine.rs::State`、catalog/Pending、`gc/reachability.rs`、`materializer.rs::plan_required`、`directive.rs`、scope promotion、quota 计算。
- 统一按照 item_id 解析唯一 owner；Pending 正文参与终态更新、required 查找、admit/derive/lease/hint 的支持或类型化拒绝。不得一边搜索称它存在，一边对同 ID required 回 Missing；不得静默成功一个实际没执行的控制动作。
- 必要回归：真实不可写 store 使记录进入 Pending；在其中执行明确 supersession、同 task/probe 的 VerifiedFixed、PromptRequired、scope close；磁盘恢复后外置/召回不恢复旧语义。checkpoint 在失败期间往返，owner 恰好一个；quota 计算覆盖所有正文位置。
- 停止条件：故障前后的同一记录具有一致语义、身份与检索结果。不要删除 Pending 来伪造有界。

## CTX-7 / P2：外置正文读回采用当前 owner 元数据

**用户结果：** inspect、fetch、admit、GC recall 对同一个条目显示一致的作用域、保留状态和来源。

- 入口：`scope.rs` 的 stored promotion、`engine.rs::fetch_external`、`directive.rs::apply_admit`、full GC recall、required/foreground stored reads。
- 明确哪些字段是不可变 blob 内容/创建身份，哪些属于当前 owner。通过 checksum 后，把当前合法元数据合入读取结果；不能让外置时的旧 scope/retention 覆盖之后的提升，也不能由 blob 的 Live 覆盖 owner 的终态。
- 必要回归：Stored 条目提升父作用域后分别 fetch/admit/required/GC recall；再 maintain/gc/restore。内容摘要一致、创建时钟不变、提升字段保留，终态拒绝仍生效。新增回归应测整条移动链而不是单测 entry 字段。
- 停止条件：移动正文不改变已提交的语义元数据，也不靠重置创建时间续命。

## CTX-8 / P2：故障时的总驻留和 full GC 工作预算

**用户结果：** 磁盘长期不可写时，Agent 给出明确背压和恢复方式；恢复后能继续，而不是让 GC 吃掉内存与响应时间。

- 入口：full GC plan/io/commit、Pending、GC report/diagnostics；Runtime 背压接线由 B 合入。
- 对 Pending 总条目/字节、一次计划序列化字节、I/O 次数与结果收集设硬界；剩余 owner 留存，游标公平续做。到无法安全接纳的边界前拒绝/让出，并类型化暴露失败、延期与积压，不能只是显示 Warm 小于 cap。
- 将 sweep 内 `marked.contains` 等已确认线性重复查找改为等价集合查询，保留输出/选择顺序；避免先 clone 全部正文再限额。不变更 GC 策略来获得好看数字。
- 必要回归：持续多轮 store 故障＋新输入，实际总驻留上界和背压；恢复后 drain；取消在计划/I/O/提交各边界；大量 roots 的整个 full pass；报告收集前后峰值与 omitted。执行数量计数优于脆弱的时间断言，实际延迟另记录。
- 停止条件：错误/延期可见、总量有界、不丢已接纳内容，恢复可继续。无需构建通用工作调度器。

## CTX-9 / P2：关闭作用域与历史索引的有界生命周期

**用户结果：** 很多工具调用/任务结束后，Context checkpoint 和材料化时间不随全部历史无限增长，必要历史仍可按需找回。

- 入口：ScopeTree/Scope、ExternalMap、ContextCatalog、context checkpoint 与 store；与 B 的任务退休规则协调。
- 先定义可安全退休的集合：没有 live/恢复引用的 closed scope、可转为有界持久描述的历史元数据。保留活跃祖先、终态依据与恢复闭包；当前 `task_completed` 依赖 closed Task scope，退休后不能把完成任务误判为未完成。
- 优先使用既有 context store 的可恢复引用/分页或显式容量背压；不新建数据库、TaskGraph 或第二套目录权威。Cold→External 枚举变化不能当成内存已释放。
- 必要回归：1 万次 open/close 工具 scope、跨很多任务、保留 checkpoint 跨退休边界；正文数量固定时测 metadata/checkpoint/扫描量；退休后无悬空 parent、旧完成任务不自动复活、历史按 ID 可读或明确保留期状态。
- 停止条件：热元数据与单次访问有预算，历史留存和退役策略可解释；不是清空 scope 或让 GC 永久停止。

## 验证与交付

每片先写用户动作和失败反例，再改实现、跑相关 crate 回归，达到验收即交付；共用既有 CI。报告实际命令、结果、默认 profile 是否接通、未验证项。单元辅助函数通过不等于真实 Runtime 往返通过。

真实长任务与成本统一交原 COST-5：保留每次根 miss、重复读取、失效证据、GC 工作量/峰值、checkpoint 大小及 main/maintenance 费用。默认 Rolling 与显式 Dynamic 分别标注。不得冻结 Focus、保留过期正文、复活语义终态或增加 filler 追求缓存命中。
