# 可执行任务队列

## 当前：M18 第三轮执行者、长期运行与可维护性续接（2026-09-13）

**仍是 M18，同质量、长期稳定、高效可靠与全成本下降。** 当前基线 `685b6bbb` 加实际未提交树。第三轮 14 项（4 P1＋10 P2），9 项有界反例、5 项源码核实；本轮没有改产品代码，也没有做真实 provider 或完整 Rust/.NET/CI 验收。[报告](reviews/2026-09-13-executor-maintainability-audit/REPORT.md) 与 [覆盖/证据](reviews/2026-09-13-executor-maintainability-audit/COVERAGE.md) 是本表依据，下面前序表仅保留时点回执。

| 归属/顺序 | 功能切片 | 用户结果与验收边界 |
|---|---|---|
| A 首片 | **CTX-10** | 退休 scope 不被旧 blob 带回，召回后 checkpoint 可恢复；退休事实环不改变完成语义。R3-02/04 **代码落地（2026-09-13，工作树）**：merge 规则 entry 无条件胜出（None=显式释放，悬空引用不可能；pre-scope 旧行降级 legacy 推断）；退休环时间序满员淘汰最旧＋`retirement_ring_overflowed` 置位；`scope::completion_facts` 保守完成语义（溢出后无 live scope 且无注记=未知=保守已完成，不恢复自动召回）。回归 2（红检查：召回带退休 scope→restore 报 missing scope；513 次循环后最新事实保留＋古老正文不召回）。[回执](reviews/2026-09-13-executor-maintainability-audit/CTX10_TO_CTX12_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 第二片 | **CTX-2 残余** | 修改超时日志仍保留五秒超时；永久撤销需要具体依据，不继续堆词表。R3-03 **代码落地（2026-09-13，工作树）**：`names_replaced_object` 收窄为替换宾语**全部**内容词（双侧排除停用词/路径词）命中旧决策；维度词共享不再授予 Superseded，不确定即 Live；未增关键词。回归 2（红检查：R3-03 反例四 owner 全 Live＋全宾语对照照常撤销；35+33 项既有撤销回归保持）。[回执](reviews/2026-09-13-executor-maintainability-audit/CTX10_TO_CTX12_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | **CTX-11** | foreground/required/fetch 复用四种 owner 与当前 metadata；Pending 不漏、Stored 不退回旧属性。R3-05/06 **代码落地（2026-09-13，工作树）**：`plan_foreground` 补 Pending 查找（store 故障期当前文件不再 Missing）；`ForegroundPlanItem::Store` 携带 plan 时 owner 快照，读回经同一 `reattach_owner_metadata` 合并——五个 blob 读回路径统一。回归 1（红检查双腿：撤臂→Missing；撤合并→帧带旧 Working）。[回执](reviews/2026-09-13-executor-maintainability-audit/CTX10_TO_CTX12_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | **CTX-12** | 无效候选不消耗有效召回额度，保持既有评分和默认策略。R3-07 **代码落地（2026-09-13，工作树）**：warm 循环先判定 `reactivation_reason` 再扣 `remaining`——无效候选零消耗；anchor 独立上限不变；扫描仍受既有批次边界。回归 1（红检查：预算 1 时无效候选先行→reactivated=0）。[回执](reviews/2026-09-13-executor-maintainability-audit/CTX10_TO_CTX12_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| B 首片 | **EXEC-9** | service Context 透传保留 checkpoint 保护根；未知不当空集，旧正文不会被 reconcile 误删。R3-01 **代码落地＋验证（2026-09-13，工作树，B 线会话；async 保护根接口的 A 线测试适配已同步）**：adapter/wire 透传＋service.rs 180 行回归。回执见 [B_LINE_ROUND3_RECEIPT.md](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI | **代码落地（2026-09-13，工作树）**：wire 增 `CheckpointRecoveryItemIds`/`StorageGcProtecting`/`ReconcileStoreProtecting` 三操作，适配器三方法走真实服务解析；`checkpoint_recovery_item_ids` 改 async+Result（解析失败=不完整→删除延期）；actor 与 spawned-boundary 根枚举收敛为单一实现。服务进程回归：外置→checkpoint A→Admit→checkpoint B→restore B→A 根恰一、未知根/不完整延期、保护性 reconcile 后 restore A 正文逐字节读回（红：适配器空集时失败）。[回执](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI |
| B 第二片 | **EXEC-10** | gc_work 单槽准入覆盖 capture/terminal/restore/stop，每份回执可结算，恢复不续接旧事务。R3-09/10 **代码落地＋门控回归执行（2026-09-13，工作树）**：spawn_gc_op 单槽 vacancy check（busy 类型化拒绝、continuation 归还）；prepare_restore 对 parked TerminalFreeze/prepare 确定性拒绝（旧事务不触恢复态）。门控回归 `restore_is_refused_while_a_terminal_commit_is_parked`＋`two_concurrent_checkpoint_captures_both_settle_deterministically` 均绿；actor/turn 全量 **143/143**。回执见 [B_LINE_ROUND3_RECEIPT.md](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI | **代码落地（2026-09-13，工作树）**：`spawn_gc_op` 单槽空位检查，占用返回 typed busy 并把续体交还（capture→确定性拒绝；终局冻结→回滚路径；turn-scoped→不可达内联兜底）；`prepare_restore` 在停靠 commit 事务/prepare 存在时类型化拒绝 restore（旧事务永不触碰恢复后状态）。门控回归：restore 停靠期被拒＋放行后 completion 正常结算；并发两份 capture 均有确定回执。[回执](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI |
| B 后续 | **EXEC-8 残余** | 冷结果查询总扫描字节/单行/等待有界，Actor 与 journal 仍能响应。R3-11 **代码落地＋验证（2026-09-13，工作树）**：`read_trace_tail` 尾部有界窗口（非整文件扫描）；storage **26/26**（含字节上界回归）。其 TaskCompleted{artifacts,final_output_digest} 契约的 replay/eval/protocol 测试构造由集成窗口机械收口（10 处字面量补齐＋status.rs match 臂修复），workspace 全量编译解除。[B_LINE_ROUND3_RECEIPT.md](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI | **代码落地（2026-09-13，工作树）**：`read_trace_tail` 改为文件末尾字节窗口（8 MiB 扫描上界）＋单行 1 MiB 上限（超限 typed 错误 fail-closed），窗口未达文件头即如实 `complete=false`；读取移出 journal writer 循环（flush 后 spawn_blocking）；Actor 冷查询命令改 spawn 任务＋reply（命令分支不再等扫描）。回归：200 行日志 max=50 → 窗口/不完整如实；超大行 → typed 错误。[回执](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI |
| B 后续 | **CTX-8 Runtime 接线** | 持续存储故障不继续积累正文；背压状态可查询、可修复后继续。R3-08 **代码落地＋验证（2026-09-13，工作树）**：`store_backpressure` 状态（观察/上报/tools 输入闸/完成边界解除）入 status 快照；回归 `store_backpressure_is_observed_and_lifted_on_the_status_snapshot` 绿。[B_LINE_ROUND3_RECEIPT.md](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI | **代码落地（2026-09-13，工作树）**：边界 pass 的 `externalize_backpressure/externalize_deferred/store_io_failures` 记入 actor 状态并上 `RuntimeStatusSnapshot.store_backpressure`（可再取的快照事实）；背压激活期间跳过 working-set 预热旁路（工具结果投递不受影响），清洁 pass 解除。门控回归：背压 pass → status active=true（deferred=7/io=2），清洁 pass 后 active=false。[回执](reviews/2026-09-13-executor-maintainability-audit/B_LINE_ROUND3_RECEIPT.md)；未提交/未跑远端 CI |
| C 首片 | **COST-7 残余** | provider 失败时已知 usage 不降成 Unknown，重试保留逐次已知/未知并去重。R3-12 | **代码落地（2026-09-13，工作树）**：新 `AgentError::FailedWithUsage{usage,source}`（唯一共享失败用量表达，`failed_with_usage` 构造器对空信封原样放行，`failure_source()` 供分类看穿）；Chat/Responses 全部失败出口先读 accumulator usage 再失败；三条重试路径 give-up 携带最近已知 usage 且包装的 Transport 失败仍可重试；`OperationOutcome::Failed` 增 usage（B 契约），actor 失败分支有证据发真实行（身份随报告、Main lane）、无证据保持 Unknown，去重队列防一笔两次。回归：provider 134（+5：length/缺DONE/Responses failed＋usage、无 usage 原错误）、retry give-up 保留＋包装可重试、runtime stream 7（+1 失败 Observed 真实行）。[回执](reviews/2026-09-13-executor-maintainability-audit/COST_R3_C_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| C 第二片，A 单一改 Rolling | **COST-8 残余** | 退避绑定真正发送的 FoldPlan，追加未发送尾部不触发相同失败调用。R3-13 | **代码落地（2026-09-13，工作树）**：只读 `plan_fold_packing -> FoldPacking{consumed,partial}`——退避摘要与实际取料共用同一装箱计划（digest＝prior id＋实际整取 id＋切分 id/前缀长）；追加进不了 source 的候选不解退避，实际输入变化仍立即重试；失败还源、partial 身份、单 pass 上限与默认值保留。回归：未发送尾部追加只调用一次（旧代码必红）、真实 source 变化恰两次调用、既有退避/冷恢复回归保持。baselines **25/25**。[回执](reviews/2026-09-13-executor-maintainability-audit/COST_R3_C_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| C 后续 | **COST-9** | GUI 先累计费用事实再丢弃渲染行，64 条 render cap 不吞计费事件。R3-14 **代码落地＋验证（2026-09-13，工作树，C 线会话）**：固定大小成本累计器先于渲染队列丢弃；shed 行费用事实仍入账。dotnet **123/123**。[COST_R3_C_LINE_IMPLEMENTATION.md](reviews/2026-09-13-executor-maintainability-audit/COST_R3_C_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI | **代码落地（2026-09-13，工作树）**：固定大小累计字段成为费用事实权威，日志行改有损投影；渲染队列弃行前先提取 model_used/context_compacted 事实入账（与活渲染共用同一累加器恰一次），摘要经 `_ui.Post` 刷新，新增 `_costRenderRowsShed` 随纪元重置；未移除 cap、未建无界队列。回归：model_used 与 context_compacted 被挤出 64 cap 后合计仍完整且不重复（旧代码必红）。dotnet **123/123**。[回执](reviews/2026-09-13-executor-maintainability-audit/COST_R3_C_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| 既有后续 | **CTX-9 / N7 元数据有界** | ExternalMap/Catalog/checkpoint 总历史工作与资源按原残余推进，不因 scope 节点退休而关闭 **checkpoint external tail spill 已落地（2026-09-13，工作树）**：checkpoint 的 external 尾部超目标即分片（inline 数组＋`external_spilled` 卡片，capture-time 元数据、缺失卡降级不失败、recovery roots 覆盖 spilled ids）；context-simple **376**（external_spill 4 项回归）。回执见 [CTX9_RESIDUAL_CHECKPOINT_SPILL_IMPLEMENTATION.md](reviews/2026-09-13-executor-maintainability-audit/CTX9_RESIDUAL_CHECKPOINT_SPILL_IMPLEMENTATION.md)；未提交/未跑远端 CI | **首片代码落地（2026-09-13，工作树）：checkpoint 外置尾分片**——超额最旧 External 条目元数据卡片写既有 store（内容寻址幂等、单 capture 写预算、IO 失败保持内联），checkpoint 只带内联段＋`external_spilled` 寻址清单；restore 查重（spilled∩owned fail-closed）后全量重水化，缺卡诚实计数不失败；`recovery_item_ids` 并入分片 id（保护根闭合）。搜索/召回/目录零改动（条目仍全驻内存）。回归 4（分片/幂等/capture 时点元数据/缺卡降级/保护根；红检查＝目标置 MAX 复现旧行为）。context-simple **376/376**、baselines 25、fmt/clippy 0。[回执](reviews/2026-09-13-executor-maintainability-audit/CTX9_RESIDUAL_CHECKPOINT_SPILL_IMPLEMENTATION.md)。**第二片（同日续）：卡片生命周期闭合**——卡片移 `cards/` 子目录与 blob 命名空间分离；启动 reconcile 孤儿清扫（保护规则与 blob 镜像：map∪保护根之外的卡删除，异形卡 quarantine，`StoreReconcileReport.external_cards_removed` 计数上报，serde default 字节稳定）。回归 `reconcile_cleans_orphan_cards_and_honors_protection`（恰删 9 保 1＋10 活跃）。context-simple **377/377**。**残余（唯一）**：Runtime 内存面（ExternalMap/Catalog 溢出）——需产品决策：O(spilled) 每搜索 IO、新增面向模型的完整性声明、或压缩驻留索引，三者取舍未定，不偷工；未提交/未跑远端 CI |
| 联合，C 汇总 | **COST-5** | 既有真实长任务、同质量全成本与恢复/资源对照；本轮 NOT_RUN，不另建评测框架 | **EXECUTED（2026-09-13，有界双臂）**：固定三任务 harness 双臂真实运行（default vs COST-8 预算旋钮），两臂产物验收全 PASS、取消/恢复编排真实执行、账目完整（两臂均无 unknown 行）；COST-6 桶语义在真实 wire 呈现（读有报告、write/miss 如实缺席）。**降本不声明**：两臂压缩事件均为 0，预算杠杆未参与，Token 差异属方差。降本判定待压缩密集固定任务的下一配对窗口。[运行回执](reviews/2026-09-12-gc-core-followup/COST5_BOUNDED_RUN_RECEIPT.md) |

**三个完整任务包：** [A 上下文与 GC](reviews/2026-09-13-executor-maintainability-audit/TASK_A_CONTEXT_GC.md)、[B 执行核心与恢复](reviews/2026-09-13-executor-maintainability-audit/TASK_B_EXECUTION_RECOVERY.md)、[C 成本与接入](reviews/2026-09-13-executor-maintainability-audit/TASK_C_COST_CONNECTIVITY.md)。本次实施首片为 **CTX-10 / EXEC-9 / COST-7 残余**，每条线一次只交付一片；本轮审查没有自动开始这些产品修复。

**复用与所有权：** A 维护 Context 状态与 GC（Rolling 状态/取料也由 A 单一编辑）；C 负责 provider/eval、成本语义及费用展示；B 负责 Actor/恢复/平台，单一合入共享 contracts/protocol/`command.rs`/compose/必要 DTO。COST-8 的 Rolling 改动由 C 提需求交 A，COST-7 的失败事实契约交 B。修复已有分叉，不新造目录、数据库、通用调度器或每入口一套补丁；不冻结证据/Focus/GC 追求缓存。

已有 CTX/EXEC/COST 实现经定向核对后复用，不整体重开。功能开发跑定向回归，合并沿现有 CI；文档检查不能关闭产品任务。下面回执的“本次派发/首片”仅指它的历史时点。

---

## M18 第二轮安排与实施回执（2026-09-12，当前由顶部第三轮续接）

**仍是同一个 M18，目标为长期稳定、高效可靠、同质量全成本下降。** 本轮只审查与安排，没有改产品代码或运行 Rust/.NET/真实 provider。基线 `685b6bbb` 加当前未提交树；相对上一轮 70 个文件变化、1 个新增。对 418 个源码/测试/构建文件完成全量盘点和词法扫描，深入追踪 GC、四种 owner、检查点、恢复与成本；未逐行全覆盖。发现 12 项源码问题（5 P1＋7 P2），见 [第二轮报告](reviews/2026-09-12-gc-core-followup/REPORT.md) 与 [覆盖边界](reviews/2026-09-12-gc-core-followup/COVERAGE.md)。

**三个执行入口：** [A 上下文与 GC](reviews/2026-09-12-gc-core-followup/TASK_A_CONTEXT_GC.md)、[B 执行核心与恢复](reviews/2026-09-12-gc-core-followup/TASK_B_EXECUTION_RECOVERY.md)、[C 缓存与成本](reviews/2026-09-12-gc-core-followup/TASK_C_COST_CACHE.md)。原 CTX-1/2/3、EXEC-1、COST-1/2/3/4 的实际修复定向核对后复用；局部实现/测试通过不代替本轮发现的组合边界。

| 负责人/顺序 | 切片 | 用户结果 | 依据与依赖 |
|---|---|---|---|
| A 首片 | CTX-5 | 当前必需 Live 证据跨 TTL/GC 仍受保护，死证据不复活 | R2-03 **代码落地（2026-09-13，工作树）**：唯一 live 保护判定 `anchor_claim_defers_expiry` 贯穿驻留 TTL/ttl×4、warm 老化、full sweep 三处终结判据（当前投影整集替换、终态不复活、StorageRequired 不延长驻留）；warm/Stored 声明召回独立预算不被启发式预算饿死；`ContextGcReport.anchor_root_misses` 显式上报未满足义务。回归 7 新增（红检查 4 项核心全红后转绿）。context-simple **366**（含续接段）、contracts 174、baselines 23、fmt/clippy 0。[回执](reviews/2026-09-12-gc-core-followup/CTX5_TO_CTX9_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 第二片 | CTX-6 | Pending 正文也能正确终结、材料化与受控召回 | R2-04 **代码落地（2026-09-13，工作树）**：Pending（外置重试列表）接入全部语义路径——四个终态队列扫描＋`apply_terminal_semantic` Pending 臂、required 计划覆盖（搜索存在与 required Missing 的矛盾消除）、admit/derive/lease/gc_hint/tag 真实执行且配额跨正文位置、scope close 原地提升；真实不可写 store 跨 checkpoint 恰一 owner，磁盘恢复后 drain 携带当前语义。回归 10 新增（红检查 7 项语义全红）。[回执](reviews/2026-09-12-gc-core-followup/CTX5_TO_CTX9_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | CTX-7 | Stored 读回不覆盖当前作用域/保留等元数据 | R2-05 **代码落地（2026-09-13，工作树）**：唯一合并规则 `store::reattach_owner_metadata`（blob＝内容/创建身份权威，entry＝当前状态权威，tags 并集防复活、entry scope_id=None 不重打）接入 fetch/admit/recall 三个读回点。回归 4 新增，整条外置→提升→读回→restore 链验证（红检查 4/4 全红）。[回执](reviews/2026-09-12-gc-core-followup/CTX5_TO_CTX9_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | CTX-8 | 外置长期失败时总驻留/full GC 工作有界，明确背压 | R2-06 **代码落地（2026-09-13，工作树）**：pending 条目硬上限（满员延期不丢弃＋`externalize_backpressure` 类型化背压）＋单 pass 外置批预算（`gc_externalize_batch`=64，公平 FIFO）；`store_io_failures` 计数不再吞错；diagnostics 增 pending_items/bytes；sweep `marked.contains` 改 HashSet（O(items+marks)，顺序不变）。Runtime 背压接线归 B 线。回归 4 新增（持续故障 6 轮上界、恢复后按批 FIFO drain、失败计数恰为尝试批、2000 条目单趟有界）。[回执](reviews/2026-09-12-gc-core-followup/CTX5_TO_CTX9_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | CTX-9 | closed scope 与历史索引有界且可恢复 | R2-07 **代码落地（2026-09-13，工作树）**：closed 且零引用且后代皆可退休的 scope 节点退休（自底向上整链一趟，无悬空 parent），`RetiredScopeNote` 有界事实环（512）保留完成事实——`task_completed` 经 `task_completion_recorded` 在退休后仍成立，完成任务不自动召回，重开任务清注记；scope close/外置两处释放非提升成员 chain stamp；`scope_retire_target`=1024＋`scopes_retired`/`retired_scope_notes` 可观测。1 万次工具 scope 开闭后树 ≤128、checkpoint <512KiB。回归 4 新增（红检查）＋既有 stamp 断言按新语义改准 2 处。残余：ExternalMap/Catalog 全量元数据分页属后续（需 Runtime 背压配合）。[回执](reviews/2026-09-12-gc-core-followup/CTX5_TO_CTX9_A_LINE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| B 首片 | EXEC-5 | 超过 64 个完成任务后仍生成合法 checkpoint 并继续恢复 | R2-01；64 task/256 receipt 冲突 | **代码落地（2026-09-12，工作树）**：`bounded_hot_pair_window` 单一投影——完成任务行与完成记录成对淘汰，`enforce_hot_bounds` 与 `prospective_terminal_snapshot` 共用（终局 checkpoint 承认的形状＝提交后内存形状）；孤立回执投影出局（legacy 恢复防御），256 记录上界降为 legacy backstop。红-first 7 项单测（含真实 `RuntimeCheckpoint::validate` 门与 64/65/66/256/257/1000 边界）＋真实 actor 全链回归（66 次完成→逐次 validate→真实终端 checkpoint 解码→冷恢复→再完成→validate）。[回执](reviews/2026-09-12-exec-core-b-line/RECEIPT.md)；未提交/未跑远端 CI |
| B 第二片 | EXEC-6 | typed 恢复引用不从正文授予读权限，坏 Unicode 引用不 panic | R2-02 | **代码落地（2026-09-12，工作树）**：`protected_runs_from_checkpoint` 只在 decode+validate 后从 typed 字段以 `ArtifactLocator::parse_sealed` 提取（needle 伪引用/prose/draft/坏 id 不构成引用，无 panic 路径，cap 64）；`collect_checkpoint_recovery_roots` 收窄为存储保护集（先 decode 再读）；读授权集改由 `PendingRestore.protected_runs` 携带（恢复 checkpoint 自身的引用，无关 checkpoint 不再进入 lineage）。`RestoreEvidenceDegraded` 事实入 `RuntimeStatusSnapshot.restore_evidence_degraded`＋`WorkSnapshotResponse`（双侧校验＋fixture）。红：旧扫描器对 `artifact://run/`＋35a＋汉 实测 panic。[回执](reviews/2026-09-12-exec-core-b-line/RECEIPT.md)；未提交/未跑远端 CI |
| B 后续 | EXEC-7 | GC/检查点维护等待不阻塞取消与停止 | R2-08；复用 operation lane | **代码落地（2026-09-12，工作树）**：`OpKind::Gc`＋`gc_work` 停靠道。落道：turn-final full GC（取消=可逆 pass 干净中止，未确认→RecoveryRequired；排队输入 drain 移入真实提交尾并随终局停靠推迟）、终局冻结的 checkpoint 维护（整笔事务停靠＋`ensure_idle` 拒新变异＋resume 复原全部错误路径与回执）、完成边界（full GC＋根枚举＋storage GC 一个 spawned op，只产事件不伪称回滚）、只读 capture。门控引擎行为回归：GC 停顿期间 status/cancel 可应答且取消后无 TurnCompleted；终局维护停顿期间 status 应答、放行后 commit 到达。**如实记录**：显式 collect 与 safe-point 写入保持内联（批次续体停靠／同步耐久协议，注释写明归属）。[回执](reviews/2026-09-12-exec-core-b-line/RECEIPT.md)；未提交/未跑远端 CI |
| B 后续 | EXEC-8 | 旧完成任务退出热表后仍能有界查询/审阅 | R2-09，依赖 EXEC-5/6 | **代码落地（2026-09-12，工作树）**：`EventJournal::read_tail`（FileEventJournal writer 任务串行实现，环形缓冲内存有界、坏行 typed 封死）；`TaskCompleted` 事件增 artifacts/final_output_digest（serde default）；`TaskCompletionLookup{Hot/Retired/BeyondJournalWindow/Unknown}`＋`RuntimeHandle::task_completion`（当前 run＋有界祖先 run 分区各 4096 行窗口，新者胜；窗口不完整如实 Beyond）。平台 `work/task_completion` 路由＋.NET `TaskCompletionAsync`＋共享 fixture 双侧钉死。全链回归：ids[0]→Retired、ids[65]→Hot、随机→Unknown、冷恢复后同 Retired。[回执](reviews/2026-09-12-exec-core-b-line/RECEIPT.md)；GUI 接线归 C 线；未提交/未跑远端 CI |
| B 残余 | EXEC-4 启动入口收口 | 启动 --restore/latest 也在读取阶段限额 | R2-12，保留已修交互入口 | **已落地（2026-09-12，工作树）**：`decode_checkpoint_file` 改一柄 take(cap+1)——超界在读取阶段点名 artifact 上限拒绝（红实测：旧代码整文件缓冲后报无关 parse 错），恰 cap 过读取门进内容校验，句柄读取使 stat 后增长无关；启动与交互入口共用。agent-tui 启动 wrapper 回归＋checkpoint 单测。[回执](reviews/2026-09-12-exec-core-b-line/RECEIPT.md)；未提交/未跑远端 CI |
| C 首片 | COST-6 | cache hit/miss/write 不混淆，双协议计费字段正确 | R2-10 | **代码落地（2026-09-13，工作树）**：DeepSeek miss 进新 `ModelUsage.cache_miss_input_tokens`（None=未报告），write 只来自显式 `cache_write_tokens` 字段；Responses transport 读取显式 write（此前仅 diagnostics 可见）；不一致计数原样保留不修复，非法/溢出类型化失败；旧事件字节不改写；跨语言金样＋.NET 访问器（write/miss 可空、Role lane）。provider **129**、contracts **174**、protocol 47＋14、dotnet **120/120**。[回执](reviews/2026-09-12-gc-core-followup/COST6_CACHE_FIELD_NORMALIZATION_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| C 第二片 | COST-7 | Dynamic/maintenance/late/partial 用量可对账，完整性贯通 | R2-11 | **代码落地（2026-09-13，工作树）**：Dynamic 非零门槛移除；空摘要经 `AgentError::EmptyCompactionSummary{usage}` 保留已计费 usage（W08 拒折语义不变），cached 字段 Option 化不再拍平成 0，write/miss 全链透传；stale 完成/维护报告的用量按原身份补充，`usage_accounted_ops` 有界去重（取消＋晚到一笔一次），维护取消也留 unknown 行；`ModelUsed` 增 `role`（Main/Maintenance），eval 聚合维护侧 unknown/重试进全局下界，JSONL 观测加 transport 实例标签。context-simple **351**、baselines **22**、runtime lib **407**（相干窗口）、compose lib **38**（含新回归）、定向套件全绿。[回执](reviews/2026-09-12-gc-core-followup/COST7_FULL_CALL_COST_COMPLETENESS_IMPLEMENTATION.md)；compose 集成目标与 actor 全量待并行 EXEC-7 收口窗口（共享树在飞域）；未提交/未跑远端 CI |
| C 后续 | COST-8 | 产品可配置执行段维护额度，同源失败不反复付费 | COST-4 续接，与 A/B 协调 | **代码落地（2026-09-13，工作树）**：`MaintenanceBudget`（calls/tokens/失败退避，env 严格解析）经 `build_context_engine` 进 Rolling 配置；零预算不挂压缩器（零不发送）；rolling 同源失败退避按候选身份摘要、内容变化即失效、随 checkpoint 持久；启动横幅打印可核对预算行（默认 token 无界如实标 unbounded，单次 pass 上界、不声称执行段总费用）。baselines **23** 含退避与冷恢复预算语义回归，compose lib **38** 含预算接线与环境解析回归。[回执](reviews/2026-09-12-gc-core-followup/COST8_CONFIGURABLE_MAINTENANCE_BUDGET_IMPLEMENTATION.md)；compose 集成目标待并行线收口窗口；轻量模型切换不实施；未提交/未跑远端 CI |
| 联合验收，C 汇总 | COST-5 继续 | 默认产品真实长任务同质量降本，故障恢复与资源不退化 | 既有验收不重开框架 | **PREPARED / NOT_RUN（2026-09-13）**：配对验收脚本＋记账提取器已交付并交叉验证（对 09-11 真实证据干跑，总额逐 token 吻合回执 105,838/3,907/72,704；双臂=default vs COST-8 预算旋钮，复用固定三任务 harness 单源）。真实执行阻塞：①agent-tui/replay 因并行 B 线在飞事件字段消费点未收口无法重建二进制；②付费窗口与 `--rounds/--timeout` 上限需明确选择。[准备回执](reviews/2026-09-12-gc-core-followup/COST5_PAIRED_ACCEPTANCE_PREPARATION.md) |

**本次派发各一片：CTX-5 / EXEC-5 / COST-6。** 每片先做能暴露组合问题的定向反例，完成实施与实际验收回执即停；不能只新增测试/文档冒充修复。B 单一合入共享 contracts/protocol/command/compose/DTO 与 Actor 发射点，A/C 确认领域语义。不得擅自覆盖共享工作树，不从裸 HEAD 漏掉前序未提交实现。

**验收重点：** 完成任务 64/65/66/256/257 的真实 checkpoint 往返；必需证据跨老化与所有正文位置；持久 store 故障恢复；GC/保存中取消；旧结果可查询；只在费用分桶正确、main/maintenance/retry 账目完整的同起点对照中宣布降本。引用了文件、磁盘留着日志、输出集合有 cap、辅助函数测试通过，分别不等于可信读授权、产品可审阅、总资源有界、组合链通过。

保持 Actor/Core 权责、GC 语义终态、有效工具与证据新鲜性；不冻结上下文追求缓存、不加 filler、不新增状态数据库/通用调度器。开发用相关回归，集成沿用既有 CI；本轮文档检查不能关闭代码任务。

---

## M18 第一轮安排与实施回执（已由顶部第二轮队列续接）

**本轮已完成审查与编排，以下功能尚未执行。** 审查基线为 `685b6bbb` 加已有未提交工作树；20 个 Rust crate、SDK/桌面及构建入口共 417 个源码/测试/构建文件完成盘点与词法扫描，关键路径逐段追踪，**未逐行全覆盖、未运行 Rust/.NET 测试或真实 provider**。8 项源码问题为 3 P1＋5 P2；证据和限制见 [审查报告](reviews/2026-09-12-executor-audit/REPORT.md)，实施步骤/验收/停止条件见 [三部分任务书](reviews/2026-09-12-executor-audit/TASKS.md)。

下一大阶段分 **A 上下文与长期记忆、B 执行核心与恢复、C 缓存与成本**。平台/GUI 的相关消费面随功能切片接通。M17 与 9 月 11 日工作树的集成和真实使用验收继续保留，不因新阶段命名自动关闭；前序实现核对后复用，不重开整套 CORE/PLATFORM/GUI。

| 顺序/负责人 | 工单 | 用户结果 | 前序映射/依赖 |
|---|---|---|---|
| A 首片 | CTX-1 | 未知范围不再让已选正文被错误省略；最终覆盖/定价/消费一致 | CORE-1/F01 残余，E01 | **代码落地（2026-09-12，工作树）**：删除 prompt.rs `omit_selected_file_body` 与 materializer.rs `price_as_file_body_descriptor` 的 identity-only 退路——`visible_body_windows_cover` 成为唯一省略/定价依据（无窗口=不省略，宁可重复不可丢失）；`ContextHints.visible_body_identities` 保留为 informational（serde default 兼容旧数据），仍由同一趟回注筛选派生。回归：E01 反例（零窗口同版本记录保留）＋无关文件窗口＋required 记录保留（均核对最终请求正文）；改写 2 项固化旧行为的 entity 测试为窗口语义；既有不相交/包含、裁剪、缓存未回注、旧 checkpoint 回归全保持。prompt:: **38**、context-simple **321**、fmt 通过；clippy 残留告警全在 EXEC-1 并行在途 actor 文件，不归属本片。[回执](reviews/2026-09-12-executor-audit/CTX1_FINAL_VISIBLE_COVERAGE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 第二片 | CTX-2 | 日志变化不撤销超时等独立要求 | CORE-2/F02 残余，E02 | **代码落地（2026-09-12，工作树）**：删除 `instead`/`revert`＋共享实体的整行捷径与「共享任意实词」分支；替代宣告必须点名替代对象（`replace <obj>` / `instead of X` / `rather than X` 宾语与旧决策内容词/实体相交），逐字 run 须含内容词且重申超集（消息逐字包含旧行）不算引用；新增保留/否定保护（keep/retain/not/still… → 并存）。`use X instead` 裸形式按 E02 授权降级为并存（需 `drop X`/`instead of X`/引用旧行）。回归 5 新增＋2 改写（stored 同判据、GC 不复活触发形式改写）；context-simple **327**、fmt、clippy 0 警告。[回执](reviews/2026-09-12-executor-audit/CTX2_EXPLICIT_WITHDRAWAL_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | CTX-3 | 摘要只声明实际收到的来源与范围；旧卡替代有依据 | E04，与 COST-4 协调 | **代码落地（2026-09-12，工作树）**：来源级装箱（成员整取或整不取、仅最新超预算来源可截断），`source_ids` 恒等于实际进入输入的来源，排除计数在卡片头 `covers N of M sources` 明示；卡片定义为累计笔记——旧 episode 卡不受 opened_tick 限制参与输入，替代仅对「实际进输入的旧卡」排队（装不进的保持 live 可检索）；失败 fallback 明示「未完成压缩」并附原文。回归 6 新增＋1 断言更新（溢出排除/预算装箱/三次旋转卡链/旧卡保留/失败明示/restore provenance 稳定）；context-simple **333**、fmt、clippy 0 警告。[回执](reviews/2026-09-12-executor-audit/CTX3_DISTILL_PROVENANCE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| A 后续 | CTX-4 | 默认长期上下文能力与产品承诺一致，缺失可操作 | CTX-1/2/3、EXEC-1；不直接换默认 | **代码落地（2026-09-12，工作树）**：①profile 决策——生产默认保持 Rolling＋baseline 最低义务（CORE-2 required_claim_misses）＋模型可读呈现，切 Dynamic 须先过真实入口旅程验收（声明落 build_context_engine 文档）；②required miss 渲染进最终请求——`REQUIRED CONTEXT STATUS (not satisfied)` 逐行 reason 类别＋item_ref＋source，恢复入口（context.search/context.fetch/artifact.read/让出操作员）收尾，8 行有界＋omitted 计数。回归 2 新增（红-first）；prompt:: **40**、compose lib 32。[回执](reviews/2026-09-12-executor-audit/CTX4_PROFILE_DECISION_AND_MISS_RENDER_IMPLEMENTATION.md)；真实入口旅程验收 NOT_RUN（需真实宿主＋provider） |
| B 首片 | EXEC-1 | 材料化等待期间仍能取消并获得可信状态 | W04 的材料化续接，E03 | **代码落地（2026-09-12，工作树）**：`OpKind::Materialize` 入既有 operation lane——轮准备在构建完整克隆 `ContextQuery` 后派生引擎调用，准备尾段打包为 `ModelRoundPlan` 驻留 Actor，完成项经代际围栏（is_stale）后以原名解构恢复（尾段语义逐字保持）；`cancel_turn` 新增 `cancel_pending_materialization`：按既有 5 秒清理上限 join，未确认即围栏（RecoveryRequired＋`TurnCommitFailed{materialize_cancel_cleanup}`），不声明可信取消、不接纳新状态；stale 晚到预览丢弃（materialize 是非消费预览，abort=文档化安全失败：锁释放、无消费、无成本）。E03 引用的 `services.rs` 内联转发器删除；Stop 经 `cancel_turn(Shutdown)` 自动获得同一有界清理。回归：门控引擎契约 5 项（挂起取消/释放与取消同到恰一终态/取消后新轮可达新 materialize/挂起时有界 Stop/存储失败围栏）——核心反例以 `EXEC1_RED_CHECK=1` 切回内联行为复现红（cancel 超时不被受理）；真引擎回归 1 项（既有 IoBoundaryPause 屏障：abort 有界结束、gate 释放、事件时钟不动、无残留 pending）。actor **86/86**、runtime lib **393/393**（完成项泵更新为跳过内部准备 op）、baselines 17、compose 全绿；fmt/clippy 本片 0 警告。如实记录：turn safepoint 1 败与 context-simple supersession 失败属并行线在飞域（新旧路径同败已归因），非本片引入。[回执](reviews/2026-09-12-executor-audit/EXEC1_MATERIALIZATION_CANCEL_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| B 第二片 | EXEC-2 | 已完成任务不使热状态/checkpoint 无限增长，旧结果可审阅 | N7 续接，E06 | **代码落地（2026-09-12，工作树）**：`TaskManager` 热驻留有界——新常量 `MAX_HOT_COMPLETED_TASK_RECORDS=64`（完成任务完整热行窗口）与 `MAX_HOT_COMPLETION_RECORDS=256`（完成记录窗口）；`enforce_hot_bounds` 在每次 Complete 提交与 restore 装入后执行（最旧先出、淘汰计数器单调、resumable 行与 active 永不受影响、oversized 旧 checkpoint 恢复进同一窗口）；淘汰不删盘（journal 完成事件＋sealed 工件按 artifact store 自身保留规则继续可审阅，窗口外查询如实返回无记录）；新 `TaskHotStateSummary`（计数＋淘汰计数器）进入 `RuntimeStatusSnapshot.task_hot_state` 诊断面。回归 4 项：MAX_HOT+10 完成淘汰窗口＋挂起 resumable 存活、300 完成记录窗口＋近期可查/早期归 journal、**1000 次合法完成后快照行数恰在双上限且序列化 < 2 MiB**、oversized 旧快照恢复入界且 active 保留。runtime lib **397/397**、actor 86/86、baselines 18、compose 全绿、clippy 0 警告。如实记录：turn safepoint 1 败与 context-simple distill 失败属并行线在飞域（归因证据见回执）。[回执](reviews/2026-09-12-executor-audit/EXEC2_BOUNDED_TASK_HOT_STATE_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| B 后续 | EXEC-3 | 多次冷恢复仍能读活跃任务的旧 sealed 快照 | CORE-3 恢复续接，E07 | **代码落地（2026-09-12，工作树）**：谱系登记改为**保护式**——`admit_artifact_run_lineage(current, predecessor, protected) -> LineageAdmission{admitted, unadmitted}`，装入顺序＝直接前代→受保护集→血统祖先（新近度）；受保护集来自 `collect_checkpoint_recovery_roots` 遍历保留 checkpoint 时对载荷 `artifact://run/<uuid>` 定位符的提取（`extract_protected_run_ids`：仅认规范定位符、畸形跳过、去重上限 64）——保护依据是「恢复状态仍引用它」，不是年代；受保护项绝不因祖先年龄淘汰，容量不足时溢出进入 `unadmitted` **类型化降级**（新契约事件 `RestoreEvidenceDegraded{unadmitted_runs}`；整个登记失败列出全部受保护前代＋Warning）——模型/SDK 首次能区分「恢复成功」与「证据可继续读取」。读取授权语义不变：非谱系前代照旧 fail closed、digest 照旧、不构成完成证明或重放授权。回归：33 次恢复后纯血统链对第一代如实拒绝（旧边界保留）而保护式登记恢复可读（**报告反例**）；40 受保护超 32 席→装入 31/点名 9 且已装入者真实可读；提取器只认规范定位符。workspace **110/110**、runtime lib **398/398**、actor 86/86、compose CORE-3 走查通过。树上并行域（COST cache-write 字段、CTX-2/3）编辑中间态如实记录。[回执](reviews/2026-09-12-executor-audit/EXEC3_RESTORE_LINEAGE_PROTECTION_IMPLEMENTATION.md)；未提交/未跑远端 CI |
| B 后续 | EXEC-4 | 大状态载入/长 trace 审阅读入在读入阶段有界 | E08 | **已落地并验收（2026-09-12，工作树）**：三入口同句柄 cap+1 有界读——TUI `/restore`（一次 open、同句柄 take(上限+1) 读入；超界在读入阶段拒绝并点名上限字节数，stat 后增长以句柄实际读到的字节为准；恰 cap 原样通过）、workspace lineage（8 KiB+1 小额读、缺失/损坏/超界 fail-closed）、replay `run_summaries_from_files`（按行流式折叠＋1 MiB 行界＋per-file 摘要预算 64，超长行计 omitted 不吞下一行）。正常大小行为不变，截断 JSON 不冒充完整恢复。验收：replay 61/61、workspace 111、agent-tui 111 全绿。[验收回执](reviews/2026-09-12-executor-audit/EXEC4_BOUNDED_LOADS_VERIFICATION.md)；未提交/未跑远端 CI |
| C 首片 | COST-1 | 成功/失败/取消/压缩/重试都有诚实成本账目 | CORE-4 续接，E05 | **代码全部落地（2026-09-12，工作树；E05.1/2/3 三断点收口）**：E05 断点 3 收口——`RuntimeEvent::ContextCompacted` 增 `usage_identity`（serde default=Unknown，旧行零计数不读作已观测），发射透传报告原值；eval metrics 仅 observed 入 token 合计、estimated/unknown 计次不入合计；共享金样 `event_context_compacted.json` 双语言 roundtrip；GUI 压缩行按 wire 身份分类、绝不并入主调用实测合计。contracts 173、protocol 47＋13、eval 223、dotnet 117/117、桌面 build 0 警告。[回执](reviews/2026-09-12-executor-audit/COST1_IDENTITY_PROJECTION_IMPLEMENTATION.md)。**收尾半（同日，EXEC-1 落地后垂直收口）**：E05.1——`OperationOutcome::Failed` 对模型轮经共享助手发 Unknown 用量行（失败/中断不消失于账本，未知不充当零）；E05.2——rolling 维护循环失败压缩推入 Unknown 条目（保源回退不变）且移除「非零才入账」门槛（provider 未报 usage 的成功调用按其身份照常入账）。回归新增 3 项（runtime 失败轮 unknown 行、baselines 失败行钉住＋零用量入账）。actor 86、turn effects 16、contracts 173、baselines 18、compose 全目标 0 失败、clippy 0、fmt OK。[回执](reviews/2026-09-12-executor-audit/COST1_IDENTITY_PROJECTION_IMPLEMENTATION.md)。**残余与交接**：stale 完成丢弃路径的用量未入账（超 E05.1 点名范围，记残余）；`turn safepoint` 1 项既有失败为共享树跨线缺陷（EXEC1_RED_CHECK 独立复核归因成立、文件零未提交差异），归集成者/B 线；E05.4 供应商字段归 COST-2，消费端已就绪 |
| C 第二片 | COST-2 | 按供应商能力映射 cache-read/write/miss 和计费字段 | COST-1；复用现有缓存边界 | **代码落地（2026-09-12，工作树）**：Chat SSE 增 DeepSeek 顶层 hit/miss——缓存读优先 OpenAI details 拼写、回退 hit，miss 保留为 provider 自己的 cache-write 报告（未发送保持 None，绝不发明零；两种拼写并存 details 优先）；`ModelUsage` 增 `cache_write_input_tokens`（serde default＋skip_serializing_if，既有金样字节稳定）；压缩链路补齐调用事实——`CompactionOutput`/`ContextCompaction`/`ContextCompacted` 增 `cached_input_tokens`/`attempts`/`retries`（旧行 default=0=未知），`ModelBackedCompactor` 透传主 transport 缓存与尝试记账，rolling→报告→事件→GUI 全链贯通，GUI 压缩行原样呈现缓存读与尝试数。provider **119**（+2 DeepSeek 映射测试）、contracts 173、eval 223、baselines 18、protocol 47＋13、compose 12 目标全绿、actor 86、dotnet 117/117、clippy 0、fmt OK。[回执](reviews/2026-09-12-executor-audit/COST2_PROVIDER_FIELDS_IMPLEMENTATION.md)。未验收：未提交/推送、未跑远端 CI；真实端点字段观测照旧 NOT_RUN（属 COST-5）；context-simple dynamic 引擎同型「非零才入账」门槛属 A 线（接口请求见回执） |
| C 后续 | COST-3 | 减少不必要前缀变化与重复组装，保留语义和权限 | CTX-1、COST-1/2；prompt 交 A 合入 | **B 线部分代码落地（2026-09-12，工作树）**：D04——items/foreground/surface-omit 三个裁剪循环的总量求导从「每次比较全量 into_messages 克隆」改为「每次重组后单次求导」跟踪值（唯一真值保持最新组装的 input；最终拒绝与保守余量不变；末尾账目直接取跟踪值，删除最后一次重复求导）；D05——删除 actor 热路径无条件 TEMP-DBG stderr（工具面事实已在 ToolSurfacePlanned 持久事件）。D03 prompt 半归 A 线合入中（B 线已在 prompt.rs 落红-first 契约测试 `identical_items_assemble_identically_regardless_of_diagnostics_counts` 钉住目标：同 items 不同诊断计数必须逐字节同请求；census 事实已由 ContextPrepared 事件承载）。actor 86/86、kv 探针 5/5、clippy 0 警告。D04 为组装开销确定性削减，非缓存收益声明（收益归 COST-5 实测）。[回执](reviews/2026-09-12-executor-audit/COST3_PREFIX_STABILITY_BLINE_IMPLEMENTATION.md)；未提交/未跑远端 CI  **A 线合入窗完成（2026-09-12，本会话）**：D03 落地——working 消息的 catalog 计数行整体移出模型请求（留 ContextPrepared 事件/引擎 diagnostics），相同 items 请求逐字节稳定（修复并行会话 identical_items 测试的随机 id 缺陷并转绿；census 断言更新为「不得重进请求」）；D05 TEMP-DBG stderr 已由 COST 线自删。prompt:: **41**、agent-runtime lib **401**。[回执](reviews/2026-09-12-executor-audit/COST3_PROMPT_SIDE_STABLE_PREFIX.md)；provider 侧测量/D04/COST-5 归 C 线 | |
| C 后续 | COST-4 | 维护调用有独立输出/时间/重试和累计预算 | COST-1/2、CTX-3 | **首半代码落地（2026-09-12，工作树）**：请求级输出上限——`ModelRequest` 增 `max_output_tokens: Option<u32>`（serde default＋skip；`derive Default`），压缩器把 `COMPACTION_OUTPUT_CHARS` 作为请求级上限写入请求，provider 双协议在 `send_max_tokens` 协商开启时以请求 cap 覆盖 profile cap（Chat `max_tokens`／Responses `max_output_tokens`）——生成成本在 provider 侧被限制，回复后截短退为兜底；端点未协商该字段时行为逐字节不变。provider **120**（+1 双协议覆盖测试）、compose 12 目标全绿（+1 cap 透传测试）、contracts 173、clippy 0、fmt OK。[回执](reviews/2026-09-12-executor-audit/COST4_MAINTENANCE_OUTPUT_CAP_IMPLEMENTATION.md)。**后半代码落地（2026-09-12，工作树）**：独立 maintenance transport——compose 增 `try_maintenance_transport_from_env()`（`MAINTENANCE_TIMEOUT_SECS` 可选覆盖压缩 transport 的时间上界，默认 120s；同一 provider 凭据与重试预算；demo 模式忽略；缺失凭据 fail-closed 报错），`build_context_engine` 增第 4 参 `maintenance_model`（缺省回退主模型），宿主/TUI/CLI 三入口一致接线（横幅标注 maintenance transport 激活）；端到端测试证明折叠走向 maintenance 模型（失败入 Unknown 账）而主模型零调用。compose 12 目标全绿（+1 所有权测试）、provider 120、clippy 0、fmt OK。**COST-4 完成（输出/时间已控；重试预算继承既有有界默认，模型选择按 D02 留待语义回归；引擎侧单维护 token 总预算 `max_compactor_tokens_per_maintain` 已存在于 RollingConfig 默认 MAX——按运营需要收紧属 A 线运营面）。[回执](reviews/2026-09-12-executor-audit/COST4_MAINTENANCE_OUTPUT_CAP_IMPLEMENTATION.md)  **A 线后半落地（2026-09-12，本会话）**：引擎侧单维护 token 预算——`RollingConfig.max_compactor_tokens_per_maintain`（默认 u64::MAX opt-in），累计（in＋out，observed/estimated）到限即延期（候选移出前判定，deferred_folds 诚实；unknown 行由 CALL 预算覆盖、`compaction_budget_exhausted` 专指 token 门槛）；报表增 serde-default 字段。回归 2 新增（预算延期＋默认对照）；context-baselines **20**。[回执](reviews/2026-09-12-executor-audit/COST4_TOKEN_BUDGET_ENGINE_SIDE.md)；compose 产品面（时间/重试/模型选择）归 C 线 | |
| 联合验收，C 汇总 | COST-5 | 同质量真实长任务的全成本下降，取消恢复与资源边界不退化 | 承接 W 第 7 行，不重开冻结评测 |

**第一轮历史派发（当前开工改读顶部第二轮三份任务）：** [上下文执行者](reviews/2026-09-12-executor-audit/HANDOFF_CONTEXT.md)首片 CTX-1；[执行核心负责人](reviews/2026-09-12-executor-audit/HANDOFF_EXECUTION.md)首片 EXEC-1；[缓存与成本执行者](reviews/2026-09-12-executor-audit/HANDOFF_COST.md)首片 COST-1。保留此处用于对照实施回执，不重复派发已经落地的首片。

**共享契约单一合入：** B 负责 contracts/protocol/command/compose/公共 DTO 与 Actor 发射点，领域语义由 A/C 确认；A 拥有 prompt 证据逻辑，C 需要变更时由 A 合入；同一 GUI ViewModel 预约窗口，禁止双写。当前修复未提交，执行前形成可复现基线或核验工作树快照，不从裸 HEAD 遗漏他人改动。

**验收底线：** 完整质量与权限/完成语义先通过；账目按 main/maintenance/repair/retry 区分 observed/estimated/unknown；真实成本使用不重叠的计费分桶。仅 Token 降低时只声明 Token 优化，未知费用不当零。保持 Focus、新鲜性、GC 终态、有效工具与硬资源预算；不添 filler 或无限历史追求命中。本轮不调用付费模型，真实对照留给后续有界执行。

开发按本片做必要定向验证，集成沿用既有 CI；文档变化只跑 `python scripts/doc_consistency.py`。任务书中建议命令不等于已运行。完成验收即停止扩展，不建 Chronicle/TaskGraph/新 worker 调度器或第二套任务权威。

---

## 前序队列与实施回执（以下为 2026-09-11 及更早时点记录）

以下保留已有工作树实施内容和历史限制。**开工以顶部 CTX/EXEC/COST 队列为准，旧段中的“当前/待做”按其记录日期理解；已修项不重复实现，残余按上表续接。**

> 状态：**新阶段「可靠长任务工作台」三条并行线（CORE/PLATFORM/GUI，2026-09-11 切换）。** 来源：2026-09-11 执行者 LLM 视角审查（基线 `685b6bbb29275bc8ec73ce6625a94567a8b8d23d`，提交时间 2026-09-10 18:16:27 UTC，对应远端 CI run `34513313166` success）——42 个源码/文档文件部分读取，追踪请求组装、上下文、正文缓存、工具输出、任务与平台边界、宿主启动与桌面主 ViewModel；**未逐行全覆盖、未运行本地 Rust/.NET 测试**（克隆受网络限制、环境无工具链），反例为源码推导。核心结论：**下一阶段不继续「再加一套 GC、再加一层编排、再扩大缓存」，而是围绕「LLM 实际收到的证据是否准确、完整、可恢复」推进；核心、平台、GUI 并行，但三线必须使用同一套证据与请求身份，不能各自推断状态。** F01–F08（4 P1、4 P2）映射为 CORE-1/2/3、PLATFORM-1/2/3、GUI-1，CORE-4 与 GUI-2/3/4 承接已有布局/只读面板/成本记账户为后续增量。原文：[reviews/2026-09-11-audit-685b6bbb/REPORT.md](reviews/2026-09-11-audit-685b6bbb/REPORT.md)；任务书：[NEXT_STAGE_THREE_TRACKS.md](reviews/2026-09-11-audit-685b6bbb/NEXT_STAGE_THREE_TRACKS.md)；读取清单：[COVERAGE.json](reviews/2026-09-11-audit-685b6bbb/COVERAGE.json)。
> 上一队列（W 系列）**W01–W08 代码全部落地**（收口见下方「W 系列」节，CI run `34408215832` 七 job 全绿）；**第 7 行「三类真实仓库任务衡量」仍是部分覆盖**（三个小型隔离样本产物通过；大型跨 crate、严格九份独立 host 覆盖声明、真实 compactor 长等待、降本对照未验收），沿原编号继续推进，**改为在本阶段真实用户旅程中一并验收，不另立项、不重开**。M17 N 系列（N0–N8）、并行三线 A/B/C、2026-09-09 核心续审 R01–R14 全部收口，不重开已关闭项。
> 本次审查与既有修复的关系：F01 不等同 R09/W02——R09 已统一「正文可见窗口」的区间包含规则，缺口在**缓存写入侧未保留范围**；F02 是 F15/N6 的续接（F15 要求「仅明确替代目标才进终态」，本轮指出即使加入替换提示词仍缺少**被替换决策本身的身份**）。已落地组件不重做，断着的链路直接接通。
> CI 状态按 run 记录，不外推到任意 SHA。
> 剩余条件项不变：真实 provider live（无凭据写 `NOT_RUN`）、下次实际发布的 PACKAGE-01（已并入 N8）、默认启用 MCP 后的 MCP-01（E1 已覆盖声明车道的取消贯通）。

## 开始执行

只读 [CURRENT.md](CURRENT.md) 和本表当前工单，然后读该工单的实现、调用者和测试。
已完成的项定向确认后跳过。一次只做一个工单。
**四个不同事实**：代码存在、已接真实传输/产品入口、检查实际执行、真实用户场景跑通——分别记录，不互相冒充。
执行任何工单前先 `git status --short` / `git rev-parse HEAD`；审查未读过的模块现场补读，不宣称全仓已审。
本轮审查基线为 `685b6bbb`；开工前先核对相关文件在该基线之后是否已变，若问题已修则引用实际修复与回归，不重复实现。

## 权责不变（本阶段）

- **RuntimeActor**：唯一调度器，执行轮次、安全点、取消与恢复协调。
- **TaskManager / TaskAnchor**：任务身份、解释、约束与进度，继续属于 runtime；不引入第二套任务状态权威。
- **Core**：授权、操作/效果权威与日志；不新增任务规划权威。
- **ContextEngine**：选择、驻留、可恢复外存与派生摘要；**不自行撤销无法证明失效的用户约束**。
- **Platform**：鉴权后的路由、连接与可查询结果；不创建另一个任务状态机，继续是经授权的 RuntimeHandle 门面。
- **GUI**：可信状态投影与显式用户操作；不以日志文字、同名目标或超时自行判定事实。

## 三线所有权与合并规则

- **核心线**拥有 `crates/agent-runtime` 的执行/上下文/请求组装、`crates/context-simple`、`crates/context-baselines`、基础 fs/search 输出、provider 请求与 usage 语义；不改 GUI。
- **平台线**拥有 `crates/agent-host`、runtime 平台路由、IPC/连接生命周期、对外读模型、`clients/dotnet` 协议与连接；不拥有 TaskManager，不改 GC 策略。
- **GUI 线**拥有 `apps/Agent.Desktop`、正式用户工作流、面板状态、导航与呈现；不直读宿主数据库/状态目录，不绕 SDK 推测完成或授权。
- **共享契约**（`agent-contracts`、`agent-platform-protocol`、`RuntimeCommand` 对外增量、`agent-compose`、共享 DTO fixture）由**单一接口合并负责人**协调，默认由平台线负责合入，**核心语义必须由核心线确认**；三线不得各自发明版本不兼容的 Receipt/Result/ContextFrame。

## 共享小前置（不阻塞三线开工）

先提交一份短契约变更说明与共同反例 fixture，然后并行实现：

1. **证据投影**：在既有 `FileBodyWindow`、`ContextHints`、`MaterializedContext` 及输出 DTO 上补足 `resource identity`（workspace/run binding＋规范化路径＋内容修订）、`visible extent`（行/字节区间或已验证整文件）、`completeness`（full/partial/unknown＋裁剪或扫描受限原因）、`provenance`（call/item/artifact 身份）。**同版本 ≠ 同范围；范围声明 ≠ 截断后仍完整。** 该「最终可见清单」是最终请求的派生视图，不是新的持久化任务权威。
2. **请求结果身份**：在既有提交收据上加可查询路径，明确进程内与跨重启承诺，使用已有 run/epoch/client_request_id/task_id/payload 身份，**不以 goal 文本匹配**；最小结果分类 accepted / known rejected / unknown / expired。
3. **可读完的产物**：在既有 sealed artifact 身份上加 offset/cursor、next offset、eof/partial；保持每页硬上限与权限/内容摘要核对；不新建任意路径下载接口。

## 2026-09-11 队列回执（可靠长任务工作台，后续见顶部）

### A 线：核心——让执行者持续拥有准确依据

| 顺序 | 切片 | 用户能获得什么 | 对应发现 | 状态 |
|---|---|---|---|---|
| A1 | **CORE-1 正文覆盖一致性**：缓存保留结构化范围/修订/完整性/来源；覆盖证明只来自最终实际进入请求的正文；定价、去重、预算裁剪、消费观测共用同一份派生清单；二次截断同步降级覆盖 | 模型不会因读取同文件另一个片段而丢失之前需要的片段；确有重复时仍能去重 | F01（P1） | **代码落地（2026-09-11，本地未提交）**：`ProtocolBodyRow` 让缓存行携带真实窗口直达组装器；`visible_body_windows_from_parts` 不再硬造 `covers_file` 整文件声明（未知范围贡献 0 窗口；裁剪窗口不构成证明）；`visible_body_windows_for_request` 与身份集合共用同一趟回注筛选（定价集合不再可能超过回注集合）。1197 项本地通过（runtime lib 391、contracts 171、context-simple 16、context-baselines 318、workspace check、fmt、clippy 0 警告、doc 检查 OK）。[回执](reviews/2026-09-11-audit-685b6bbb/CORE1_BODY_COVERAGE_IMPLEMENTATION.md)；待远端 CI 确认 |
| A2 | **CORE-2 上下文义务与语义终结**：公共 mandatory-claims 边界或明确 Unsupported；决策撤销绑定具体旧决策及其替换依据 | 必需依据不会静默丢失；改一项要求不会顺便抹掉同文件其他要求 | F02（P1）、F03（P1） | **代码落地（2026-09-11，本地未提交）**：F02——`queue_decision_supersessions` 追加 `names_the_same_requirement`，按提示动词直接宾语区分「整实体撤销」（`use X instead`/`drop X for Y`）与「范围化替换」（`replace … logging in X with …`），文件路径相同永不单独构成证明，外部存储分支同判据；反例「改日志格式不撤销超时要求」先失败后转绿，`drop X for Y` 等既有正当撤销保持绿。F03——基线引擎不再硬编码空 `required_misses`，改为把每个 `PromptRequired` 声明如实报告为 `Missing`（`shared.rs::required_claim_misses`），生产默认 `rolling` 的未兑现义务不再被当成「已满足」；`dynamic` 引擎本就真实兑现该契约、不动。context-simple **320**、context-baselines **17** 通过；workspace check、fmt、clippy 0 警告、doc 检查 OK。另有 2 个既有回归在中途被过严判据改红、已修复并保留过程记录。[回执](reviews/2026-09-11-audit-685b6bbb/CORE2_OBLIGATIONS_AND_TERMINALITY_IMPLEMENTATION.md)；待远端 CI 确认 |
| A3 | **CORE-3 检索—补读—复用闭环**：结果正文带类型化 coverage header（范围/跳过原因/后续页/原始 artifact 与 cursor）；分页与 checkpoint 保留同一语义；串联既有检索、context lookup、artifact read | 模型知道搜索是否找全，能定位并补读具体范围，而非反复全仓重扫 | F04（通常 P2，穷尽性任务 P1） | **代码落地（2026-09-11，本地未提交）**：正文级 coverage 页脚统一——`TurnFrame` 只发 `model_content`，故 `search.grep`（含取消部分命中与快照分页）、`fs.list`（非空 PARTIAL 同类）、`artifact.read`（窗口/续页/结束标记/扫描预算截断）与 `context.search`（打满 limit）的完整性声明全部由类型化扫描值生成为 `[coverage]` 单行页脚长在正文里；溢出续读指针并入同一条页脚。8 项新回归均先复现失败再转绿；tool-runtime 261、agent-core 152、context-simple 320、context-baselines 17、compose 全套、agent-runtime lib 391 通过；workspace check 0 警告、fmt、clippy 0 警告、doc 检查 OK。[回执](reviews/2026-09-11-audit-685b6bbb/CORE3_SEARCH_COVERAGE_BODY.md)；**同日收口 checkpoint 半**——恢复走查抓到真缺陷：冷恢复后 run 换新，恢复前捕获的快照 cursor 全部被 `open_artifact_for_run` 拒绝；修复为恢复提交时 actor 经 `admit_artifact_run_lineage` 把前代 run 登记进当前 run 的谱系文件（有界 32、原子写、fail closed、sealed digest 照旧），真实组合端到端回归验证「恢复前 cursor → 冷恢复 → 翻同一快照末页 + 源文件改写不混入」。workspace 108、compose 49＋1、agent-runtime lib 391 通过。真实 provider 场景未验收，待远端 CI 确认 |
| A4 | **CORE-4 缓存与压缩的实际成本优化**：保留 CurrentStateLast 与既有布局；稳定不变段保持顺序与序列化；主请求/压缩/重试/取消统一记账，保留 observed/estimated/unknown 身份 | 长任务少重复读取、较少无效压缩，并能解释成本来自哪里 | 报告设计建议（非新缺陷） | **记账真值半代码落地（2026-09-11，本地未提交）**：`UsageIdentity`（observed/estimated/unknown）入契约；`ModelUsed` 增身份字段＋类型化 `usage`（旧行 default unknown，.NET 不消费该事件、无跨语言影响）；取消在飞模型回合显式留 unknown 账目行（不再静默丢失）；`CompactionOutput`/`ContextCompaction` 带身份，`ModelBackedCompactor` 近似推导标 estimated 不再冒充观测；eval 聚合仅 observed 计入消耗。5 项新回归（取消/压缩器两项红-first）；contracts 173、agent-eval 221、compose 50、其余基线不变。[回执](reviews/2026-09-11-audit-685b6bbb/CORE4_USAGE_ACCOUNTING_IDENTITY.md)；前缀优化/成本对比需真实 provider，照旧 NOT_RUN |

**实现落点：** `agent-runtime/src/execution/body_cache.rs`、`actor/mod.rs::record_protocol_body`、`prompt.rs`、`actor/model.rs`、`context-simple/materializer.rs`、`context-simple/engine.rs`、`gc/reachability.rs`、`context-baselines/src/{rolling,shared}.rs`、`tool-runtime/src/tools/{fs,search}.rs`，以及 broker/runtime 的二次限幅边界（`agent-workspace/src/broker.rs`）。

**做到这里停止：** 不重写 GC、不新增存储后端或向量库、不做大型语义解析体系、不新增 TaskGraph、不为提高缓存命中率冻结当前焦点/权限/工艺面。搜索性能真正成为瓶颈后，再做路径/版本驱动的增量索引，优先复用已有索引与恢复接口。

### B 线：平台——让每次操作和每份结果都可以被准确查询

| 顺序 | 切片 | 用户能获得什么 | 对应发现 | 状态 |
|---|---|---|---|---|
| B1 | **PLATFORM-1 精确提交结果查询**：既有 actor 账本上增加只读 exact-request 查询；分型 accepted/known rejected/unknown/expired；明确 256 条进程内窗口与重启边界 | 断线后能区分本次提交已受理、明确失败、证据丢失或仍未知 | F06（P1） | **代码落地（2026-09-11，工作树）**：新只读路由 `work/submit_result`——runtime 账本（256 条进程内 `VecDeque`）受理时记录 domain 分离 payload digest；`WorkSubmissionQuery`（Recorded{task_id,payload_digest,matches}/Unknown）经 `RuntimeHandle::query_work_submission` 只读暴露（无 idle fence/模型轮/checkpoint）。协议五分型 `accepted/already_accepted/known_rejected/unknown/expired`＋run 绑定＋id 回显＋事实一致性校验；runtime 只产出三态（`already_accepted` 并入 Accepted 语义、`expired` 为持久化收据承诺预留的 wire 分型，进程内无证据一律 Unknown 不冒装知道历史）。.NET 镜像 DTO＋`SubmitPayloadDigest`（与 Rust 字节一致，跨语言金样钉死）＋`IAgentConnection/AgentConnection/ResumableSession.SubmitResultAsync`（RunQueryAsync 故障重连一次重问）。回归：协议 3、actor 4（含报告反例「同 Goal 旧任务在列，未受理 id 仍 Unknown」与 256 条挤过期读作 Unknown）、host e2e wire 三臂、.NET 3（conformance 2（wire 形状＋digest 金样）＋真实宿主 `ResumableSession` 收据三态（含换 id 同 Goal 仍 Unknown））。protocol 45、actor work 22、host_e2e 8/8、dotnet 过滤 21/21 全绿；fmt/clippy 0 警告。**跨重启确认按任务书明确不实现**（持久化收据是产品承诺，不是扩容 HashMap）。GUI 消费半（删 Goal 核对文案）由 C3 承接。[回执](reviews/2026-09-11-audit-685b6bbb/PLATFORM1_EXACT_REQUEST_RECEIPT_IMPLEMENTATION.md)；待远端 CI 确认 |
| B2 | **PLATFORM-3 正式连接契约**：平台感知 transport 集中到 SDK/连接配置；统一 WorkspaceIdentity/endpoint 展示与解析；公开 host/run epoch 与协商 profile；共享协议 fixture 检查 Rust/.NET 关键字段与错误分类 | 默认连接在目标 OS 正常工作；连接指向哪个工作区与宿主清晰可见 | F05（P1，支持 GUI-1） | **代码落地（2026-09-11，工作树）**：①WorkspaceIdentity 解析规则对称化——修复「宿主 canonicalize 后哈希、.NET 按原样字节哈希」的真实不对称（相对路径/`sub/..`/符号链接 CWD 推导出不同端点）；新 `WorkspaceIdentity.Resolve`（GetFullPath＋盘根不折叠＋叶链接最终目标，失败保原形），`DefaultSocketPathFor/DefaultLocal/DefaultEndpoint` 全部改经 Resolve；哈希原语保持逐字节（金样钉原语、规范化语义不进跨平台 fixture），宿主新增 `sub/..` 等价回归。②快照公开 run epoch＋工作区身份——`WorkSnapshotResponse` 新增 `run_id`＋`workspace_root`（canonical 展示形，剥离 Windows verbatim 前缀；校验非 nil/4096 字节 opaque），宿主从 `RuntimeStatusSnapshot`/`Workspace::root()` 填充；快照金样双侧同步；相等性权威＝快照里的宿主 canonical root（客户端 best-effort 解析不冒充逐位一致）。③版本漂移拒绝已由每帧信封 protocol 校验满足，不复制第二真相；Windows 共享管道保持既有 DACL＋首实例设计，误连由快照 `workspace_root` 比对闭合。回归：协议 1＋宿主 1＋e2e 双平台断言＋.NET 2＋六个测试双体快照载荷补齐身份字段（SendAsync 接收路径校验诚实拒绝缺字段应答）。protocol 47、fixtures 10、host lib 8、e2e 8/8、runtime actor 80、dotnet **108/108** 全绿；fmt/clippy 本切片文件干净。GUI 面板呈现归 C 线。期间并行 CORE（RuntimeEvent usage）与 PLATFORM-2（artifact 分页）在同一批共享文件落码，窗口后全树检查不归属本切片，回执有记录。[回执](reviews/2026-09-11-audit-685b6bbb/PLATFORM3_CONNECTION_CONTRACT_IMPLEMENTATION.md)；待远端 CI 确认 |
| B3 | **PLATFORM-2 完整结果与证据读取**：artifact 扩展为有界分页（绑定原始 run 与 sealed digest，返回位置与 eof）；changes 读模型补可定位的修订/产物引用；context 读模型区分在存储/驻留/本轮实际发送/仅摘要指针 | 能读完大型产物、从变化摘要定位实际内容，并看到信息的新鲜度与完整性 | F08（P2 功能缺口） | **artifact 分页半随 GUI-2 垂直落地（2026-09-11，工作树）**：`WorkArtifactRequest.offset`／`WorkArtifactResponse.offset`＋`next_offset`（恰在 truncated 时出现）＋收紧的双侧一致性校验；runtime 按 offset seek 有界读、越界结构化拒绝；sealed 身份逐页核验不换版本；host_e2e 分页序列＋重组逐字节一致＋越界拒绝 8/8；.NET DTO/连接面镜像。`changes`/`context` 读模型两半已收口（2026-09-11）：`ChangeSummary::MutationPrepared.old_content_artifact` 把捕获 before-body 溢出为 run 内 sealed 引用（内容寻址去重、失败降级 None，changes→分页读回原文闭环）；`ContextItemSummary.residency`＋`selected_current_turn` 区分驻留/存储/本轮实际发送/仅摘要指针（context-simple inspect 统一盖章、baselines Resident、.NET 镜像可空）。[回执（两半＋分页验收）](reviews/2026-09-11-audit-685b6bbb/PLATFORM2_EVIDENCE_READING_IMPLEMENTATION.md)、[GUI-2 回执](reviews/2026-09-11-audit-685b6bbb/GUI2_REVIEW_WORKBENCH_IMPLEMENTATION.md) |
| B4 | **PLATFORM-4 跨层事实与协议一致性**：维护共享接口版本与有限跨层 fixture；核心的 evidence completeness 与 usage 来源可被 SDK/GUI 原样读取；对已有指标与恢复事件做增量字段 | SDK 与 GUI 不必从日志文字推断事实 | 报告设计建议 | **代码落地（2026-09-11，工作树）**：①usage 原样读取——核心线 CORE-4 加进 `RuntimeEvent::ModelUsed` 的 `usage_identity`（observed/estimated/unknown）与 `usage` 详细报告经平台事件通知的 `RuntimeEventEnvelope` 原样转发；.NET 新增类型化访问器 `TryGetModelUsage(out ModelUsageFact)`（计数＋attempts/retries＋identity，`IsObserved`/`IsIndeterminate` 强制区分实测与推计——「丢失 usage 的取消不得显示成零消耗」在 SDK 层有类型化表达；非 usage 事件返回 false）。②跨层 fixture——新增共享金样 `event_model_used.json`（完整 model_used 通知帧），Rust `event_fixture_pins_model_usage_facts`（解码内核类型化信封＋断言 `UsageIdentity::Observed`）与 .NET `Event_fixture_pins_model_usage_facts`（逐字节 roundtrip＋访问器）双侧消费。③随附修复 wire 卫生缺陷——.NET 事件信封的四个派生只读属性（`EventType`×2、`IsLiveOnlyProgress`×2）此前会被序列化进输出（客户端侧重序列化事件帧即注入 Rust 侧不存在的字段），全部 `[JsonIgnore]` 并由新 fixture 逐字节 roundtrip 钉住。④PLATFORM-2 跨层缺口补全（同回执）——并行会话的 `old_content_artifact` 引用未进 .NET converter 白名单，真实宿主 changes 应答会被 .NET 拒收；已修复＋校验上限＋conformance 三事实钉住。共享接口版本与错误分类的每帧校验经 PLATFORM-3 核验维持。验证：protocol 47＋fixtures 11、actor 81、host 8/8、workspace check、fmt/clippy 0 警告、dotnet **112/112**、doc_consistency OK。GUI-4 成本面板消费归 C 线。[回执](reviews/2026-09-11-audit-685b6bbb/PLATFORM2_PLATFORM4_COMPLETION_IMPLEMENTATION.md)；待远端 CI 确认 |

**实现落点：** `agent-runtime/src/work.rs`、`platform.rs`、`platform/work.rs`、`agent-host/src/{lib,main}.rs`、`agent-platform-protocol/src/work.rs`、`clients/dotnet/Agent.Client/{Transports,WorkDto,ResumableSession,AgentConnection}.cs`。

**做到这里停止：** 不新建第二个 TaskManager、不建任务日志数据库、不另建评测平台；MCP/Skills/多 Agent 的全面扩展不阻塞本阶段。

### C 线：GUI——把现有正式客户端推进成可以完成工作的界面

| 顺序 | 切片 | 用户能获得什么 | 对应发现 | 状态 |
|---|---|---|---|---|
| C1 | **GUI-1 生产入口与异步隔离**：RealHost 走平台感知选择（实际 `ConnectAsync → BuildTransport`）；统一捕获 connection epoch/selection id/request sequence，成功与失败路径同等 fencing；unknown/未送达/已受理/已执行/已完成分状态 | 默认 Linux 连接正确；旧连接/旧任务结果不会覆盖当前界面 | F05（P1）、F07（P2） | **代码落地（2026-09-11，工作树）**：F05 `BuildTransport` 的 RealHost 分支改为平台感知（Windows 命名管道／Unix 工作区 UDS），显式 endpoint 两种选择原样保留；决策抽出为可注入 OS 的静态重载，使非 Windows 分支在任何宿主可测。F07 三处：①新增 `RequestEra` 统一守卫（connection epoch＋selection 序号），四条只读路径成功/失败同一校验——旧 era 的**失败**不再清空新连接面板；②`SelectedTask` 每次变更（含置空）递增 selection 序号，旧任务详情晚到不覆盖新选择；③**era 作用域 UI 缓冲**——断开时清理 `_pendingUiEvents`/`_pendingUiDelta`，且 delta 缓冲带产出连接戳，退役 era 的 delta 正文不再在晚到的 drain 里刷进新会话输出面板（原 `DrainPendingUiEvents` 对 `deltaText` 无任何 era 检查）。新增 4 项回归，**每项均先在回退修复的代码上复现失败**再转绿。dotnet 95/95、桌面 build 0 警告。未验收：真实 Linux 宿主端到端（本机为 Windows，非 Windows 分支由决策表与注入式重载覆盖，未跑真实 UDS 连接）；unknown/未送达/已受理分状态待 PLATFORM-1 落地后承接；真实 provider 照旧 NOT_RUN |
| C2 | **GUI-2 结果审阅工作台**：保留既有任务/审批/changes/artifact/context 面板，对接分页继续阅读与明确 eof；提供身份/修订/部分完整标记；输出正文与运行日志分开呈现 | 大结果关键结论在尾部时用户仍可独立核查；重复查看不重触发模型或工具副作用 | F08（P2） | **代码落地（2026-09-11，工作树）**：artifact 分页消费面——「读取/下一页/读尾部」三命令（续读沿服务端游标、eof 失效、尾部经 1 字节探测直跳末窗），面板头部带身份/窗口区间/eof 标记；有界累积窗口（1 MiB，超限释放最旧并明示，不在 UTF-8 序列中间切）；跨页多字节字符由下一页补全（不渲染替换符）；重复查看均为只读调用。输出正文与运行日志分离——模型 delta 独占「输出正文」，回执/失败/账目/事件行入「运行日志」（同等有界保留），AXAML 双 Tab。平台依赖（artifact offset/cursor/eof）随本片垂直落地，见 B3 行。dotnet 111/111、桌面 build 0 警告。[回执](reviews/2026-09-11-audit-685b6bbb/GUI2_REVIEW_WORKBENCH_IMPLEMENTATION.md)。未验收：未提交/推送、未跑远端 CI；GUI→真实宿主分页人工走查未做（host_e2e 已含真实宿主分页序列）；「点击证据回到对应调用」深链未做 |
| C3 | **GUI-3 可靠继续与恢复**：使用 exact request 查询并删除基于相同 Goal 消除未知提交的逻辑；追加指令绑定选定 task；取消按钮区分取消请求与可信停止；重连先取可信 snapshot 再按 watermark 接续事件 | 丢 ACK、重启、旧同名任务、审批未知、取消与工具完成竞态均无虚假确定文案，也不自动重发有副作用操作 | F06（P1） | **两片代码落地（2026-09-11，工作树）**。**第一片（F06 消费半）**：删除 `ResolveOutstandingSubmitFromSnapshot` 的 Goal 文字匹配，未知提交改由 `work.submit_result` exact-request 查询解除（client_request_id＋`SubmitPayloadDigest`，快照任务列表不参与判定）：Accepted/AlreadyAccepted 解除未知并报 task id；KnownRejected 报冲突（原内容摘要前缀为证）并释放死 id；Unknown/Expired/查询失败**保持未知**、每 id 只报一次；era fencing＋回执 id 回显核对，Pending 不查询（R07 同 id 幂等重试保持）；每份快照与每个新未知触发 single-flight 查询。.NET 客户端只读消费面随片落地（`IAgentConnection/AgentConnection/ResumableSession/FixtureAgentConnection` 的 `SubmitResultAsync`；与 PLATFORM-1 重叠半边一次关闭，协议/宿主侧未动）。**第二片（任务书第 2/3/4 条）**：继续回执核对选中任务并在不一致时明说「继续的是活动任务」（wire 无绑定字段，GUI 诚实呈现，不改协议），按钮文案「继续活动任务」；新增 `CancelRequestPhase` 状态机＋状态条 `CancelStateText`——发出即「已发出等待确认」，仅类型化 ack 升级「已确认取消/没有活动轮次」，不可确定保持警告，未决阶段由可信快照重推导（在途不越权、断开作废、era 守卫拒晚到结果）；重连 snapshot→watermark 接续核对为复用既有 B1/ResumableSession 机制，无新增代码。新增 3 项 F06 回归＋5 项继续/取消回归＋2 项 restore 走查升级（均先红后绿）。dotnet 106/106、桌面 build 0 警告、doc 检查 OK。[回执一](reviews/2026-09-11-audit-685b6bbb/GUI3_EXACT_REQUEST_IMPLEMENTATION.md)、[回执二](reviews/2026-09-11-audit-685b6bbb/GUI3_CONTINUE_CANCEL_IMPLEMENTATION.md)。未验收：未提交/推送、未跑远端 CI；真实宿主 e2e 与人工 GUI 走查未做（协议/宿主侧验收属 PLATFORM 线）；continue 的协议级 task 绑定属平台契约增量；统一用户旅程验收待真实场景；真实 provider 照旧 NOT_RUN |
| C4 | **GUI-4 展示实际上下文与成本**：区分 actual sent/resident/pointer-only/stale/missing；成本显示主调用/压缩/重试并区分实测/估算/未知；随功能拆分 MainWindowViewModel | 用户能回答「为什么模型又读了这个文件」「哪些资料没有完整提供」「这次取消有没有未知费用」 | 报告设计建议 | 第三波；**GUI 消费面已代码落地（2026-09-11/12，工作树）**：①成本三分账目——`model_used` 经 `TryGetModelUsage` 按实测/估算/未知分类，实测合计只含 observed，未知轮「按未知计，不计为零」（丢回执的取消不显示成零消耗），重试记为下界；②压缩成本——`context_compacted` 按服务端报告呈现（事件未带实测身份，明确不并入主调用实测合计）；③只读 Context 每行渲染类型化新鲜度四态（驻留·本轮已发送／驻留未在最新表面／暖缓冲／存储中仅摘要指针），旧服务端缺字段显示「新鲜度未知」，绝不推断；账目随连接纪元断开重置。dotnet **115/115**、桌面 build 0 警告、doc 检查 OK。[成本与新鲜度回执](reviews/2026-09-11-audit-685b6bbb/GUI4_COST_CONTEXT_DISPLAY_IMPLEMENTATION.md)、[压缩成本补充](reviews/2026-09-11-audit-685b6bbb/GUI4_COMPACTION_COST_ADDENDUM.md)。未验收：未提交/推送、未跑远端 CI；真实宿主 GUI 人工走查未做；ViewModel 拆分随功能逐步进行、大规模纯重构不作前置；真实 provider 的金额与缓存收益对比照旧 NOT_RUN（属 M18 COST 阶段验收） |

**实现落点：** `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs` 及其面板/命令；`clients/dotnet/Agent.Client` 的只读方法消费面。

**做到这里停止：** 不换 GUI 技术路线、不另做验证壳、不把大规模纯重构设为全功能前置；GUI 不保存第二份任务/权限/完成真相，不改 Core 绕过文件写入口。

## 并行节奏与合并点

**不是「核心完成后开发平台，平台完成后开发 GUI」。**

| 波次 | 核心 | 平台 | GUI |
|---|---|---|---|
| **第一波：修真值与入口** | CORE-1、CORE-2 | PLATFORM-1（进程内精确查询）、PLATFORM-3 | GUI-1；unknown 文案与同名核对停止 |
| **第二波：形成工作闭环** | CORE-3 | PLATFORM-2、跨层 DTO | GUI-2、GUI-3 |
| **第三波：优化与解释** | CORE-4 | PLATFORM-4 | GUI-4 |

GUI 可用契约 fixture 开发界面，但每个功能必须尽快落到真实宿主链路，不以 fixture-only 宣告正式完成。重叠切片一次执行、双方同时关闭，不重复立项、不拆第二套待办。

## 统一阶段验收：一条真实用户旅程

复用仓库已有验证与回归入口，不额外建立独立任务编排或 benchmark 系统；每个新行为补对应的最小反例回归。

1. 从正式 GUI 默认连接真实宿主，提交跨文件修改任务。
2. 模型搜索到部分命中且遇到被跳过范围，明确看到不完整声明并补读。
3. 对同版本文件读取不相交片段，跨过尾部截断，再需要较早片段；它仍准确可见或可恢复。
4. 用户追加只改变日志格式的要求，原有超时/兼容性约束没有被撤销。
5. 过程中断线导致一个 ACK 丢失，重连不凭同名任务猜测，也不自动重复提交。
6. 用户从 GUI 阅读大型产物尾部和修改证据，触发一次显式取消/继续；所有状态与实际 authority 一致。
7. 完成后汇总任务结果与 observed/estimated/unknown 成本；丢失 usage 的取消不能显示成零消耗。

**通过条件：** 任务产物正确、必需证据闭合、权限/副作用边界保持、默认生产入口可用、未知状态不被假定为确定、结果可完整核查。缓存优化不得以损失任何一项为代价。

**已有验证域与实测工作继续沿原入口推进**（W 系列第 7 行的三类真实任务衡量并入本旅程验收，不新建平行评测工程），真实 provider 不可用照旧写 `NOT_RUN`。

## 文档收敛

- `CURRENT` 只写实际默认配置、最近成功路径与仍未验收的限制。
- `NEXT_TASKS` 只保留三条活动主线、接口依赖与可交付产物（本节）。
- 完成历史归档但保留证据，不删除必要证据、不让历史修复清单继续占据 coding agent 的主要工作焦点。

---

## W 系列（2026-09-10 审查 `fb1ec9c`：Agent 任务流程核心——代码全部落地，第 7 行仍有部分覆盖）

**实施后复核（2026-09-10，`8a0dc29` → `6ec044a`）：**下表保留原实施与 CI 回执，新增反例仍沿原编号执行，不另建阶段。详见 [复核与缓存设计](reviews/2026-09-10-cache-design-8a0dc29/REPORT.md)、[验证记录](reviews/2026-09-10-cache-design-8a0dc29/EVIDENCE.md)。

- **W02：已修复并包含于 `6ec044a`**。候选必须具有自己的文件/版本与完整覆盖范围；同 id 不同片段、跨来源同文本不能掩盖 required_miss。
- **W04 非 BeforeModel 取消已落地（2026-09-10，工作树）：**UserInput/AfterTool/AfterModel 维护接入既有 operation 续接，取消先推进 Core 代际再停止并 join 维护。新输入恢复入账前快照；提交阶段中断返回 RecoveryRequired，保留已应用效果，不误发 TurnCompleted。输入事务成功后使用新 directive 的执行/证明版本；停机有界等待当前提交。门控与完成/取消竞争回归见 [W04 实施与验证回执](reviews/2026-09-10-maintenance-cancellation.md)。W04(P2) 的零预算延期账目已在 `732cf93` 修复，本片不重做。
- **W06 已包含于 `732cf93`：**按渲染字节统计捕获上限，截断保留行边界，游标指向首个未展示行；本片核对 HEAD 后跳过重做。**第 7 行已完成小型基线，范围与下一片见下条**；不复跑缓存合成样本。
- **W04 dynamic ingest 已完成本地验收（2026-09-11，工作树）：**取得真实 Simple 引擎＋门控 compactor 的修复前取消超时反例后，将 ingest 与 UserInput 维护纳入同一 operation/输入事务。取消和停止先 join 后恢复完整快照，继续保留原指令；正常压缩入账/用量报告各一次；部分输入失败回滚，回滚失败返回 RecoveryRequired。Runtime 620 项及 Clippy 通过，[本片回执](reviews/2026-09-11-ingest-cancellation.md)。Flash 原样本没有触发压缩，不充当关闭证据。**下一片先准备第 7 行的严格九份独立 host 验证覆盖声明任务，固定起点、目标与产物检查后再运行有界 Flash；不重复成功样本。**
- **第 7 行首轮基线已执行，部分覆盖：**三个小型隔离真实代码样本产物通过；已知 21 次 usage、另 1 取消操作缺测。跨进程恢复和约 4 MB 工件尾部读取通过，九类应用检查不冒充九份独立 host 证明；大型跨 crate、真实 compactor 和降本对照仍未验收。[实测回执](reviews/2026-09-11-flash-workflow/REPORT.md)。**本行后续并入「可靠长任务工作台」的统一用户旅程验收，沿原编号推进。**
- **供应商 KV 缓存首切片（用户明确共同底层优先）：**按 [KV_CACHE_PLAN.md](reviews/2026-09-10-cache-design-8a0dc29/KV_CACHE_PLAN.md) 在 PromptAssembler 建立稳定段/current_view 的共同布局，稳定合法工具集合的表示、状态后置，同步记录前缀变化原因；随后有界区段，再由薄 provider 适配层映射专属参数与用量。底层不等待供应商选择；独立观测不被 W04/W06 整体阻塞，涉及提交边界才依赖其修复。不启用工具 memo、不改 GC/打分，不以离线字节前缀冒充 provider 命中率。

  **首个保守切片已落地（2026-09-10，工作树）：**`CurrentStateLast` 移动完整目录/焦点/进度块，保留全部文字、角色、选中顺序、正文去重、工具选择和协议窗口；新增布局版本及 Legacy 组合回退，request metadata 标明布局。[实施回执](reviews/2026-09-10-cache-design-8a0dc29/KV_LAYOUT_IMPLEMENTATION.md)。为保留当前焦点策略，本片不拆正文标题、不冻结选中集合、不稳定化实际已变化的 schema surface。下一步先做最终请求前缀变化归因和用量覆盖，再决定剩余布局改动；区段化与专属缓存参数尚未实施。

  **发送观测＋隔离复测已完成：**继[首次 10 次实测](reviews/2026-09-10-cache-live/REPORT.md)后，按需 provider 观测入口和 [12 次隔离预热/换序对照](reviews/2026-09-10-cache-live/ISOLATED_REPORT.md)已运行。当前 `eval.env` 可用，不再以旧“无凭据”回执作为阻塞。新布局超过 20KB 的 HTTP 前缀保持不变，但更新焦点/进度的两次请求仍为零读取；原样重放 5/6 命中，12/12 合成回答正确。由此进入下一段的共同复用边界实现。隐藏服务原因、金额和整仓任务质量未验证；不重复相同条件的付费调用，不重开长任务或冻结实验。

  **共同复用边界＋显式映射本地实现完成（2026-09-10）：**`ModelInput::into_request` 从最终 packing 的请求绑定单个 `PromptReuseBoundary`，实际正文/角色/顺序/schema 失配即失效；只在 `OPENAI_PROMPT_CACHE_MODE=responses_explicit` 且固定 Responses 协议时映射断点，默认供应商请求及 profile digest 保持。定向测试与 Clippy 通过。[本片回执](reviews/2026-09-10-cache-live/BOUNDARY.md)。**真实验收未过：2 次冷请求失败，第二次 HTTP 400 且错误提到 `prompt_cache_breakpoint`；未进入改 D 复用 E 阶段。后续小请求已查明服务端明确报告当前模型不支持该断点，见 [能力定位与类型化诊断](reviews/2026-09-10-cache-live/CAPABILITY.md)。当前默认模式保持，显式收益验收需要已确认支持的端点/模型；不重复该拒绝请求，不改焦点角色/GC/选择策略。**

  **DeepSeek Flash 合成复用验收通过（2026-09-10）：**用户指定官方 `deepseek-flash`，以 Responses＋非思考档位＋原生默认缓存完成 10 次真实请求，答案 10/10 正确。三次当前状态变化，新布局每次缓存读取 6,144/6,468（94.99%），Legacy 256/6,468（3.96%）。类型化推理档位配置与验证已落地；[实测回执](reviews/2026-09-10-cache-live/DEEPSEEK_FLASH.md)。本片完成；后续 W04 非 BeforeModel 维护取消现已在工作树接通，见上方回执。真实仓库质量/思考模式工具续跑/实际账单仍未验收，不重复本组合成实测。

来源：2026-09-10 Agent 任务流程审查（基线 `fb1ec9c`；主审查者逐项复核＋隔离反例，8 项：4 P1、4 P2）——[reviews/2026-09-10-agent-workflow-fb1ec9c/REPORT.md](reviews/2026-09-10-agent-workflow-fb1ec9c/REPORT.md)，缺陷明细与「不要做什么」见 [AUDIT_TODO.md](AUDIT_TODO.md) 2026-09-10 表，切片顺序与衡量方式见 [WORKFLOW_AND_ROUTE.md](reviews/2026-09-10-agent-workflow-fb1ec9c/WORKFLOW_AND_ROUTE.md)。W 编号只是本轮定位，不另建阶段；此前 N/A/B/C/R 队列全部收口（CI 确认记录见 CURRENT.md）。

| 建议顺序 | 切片 | 用户能获得什么 | 状态 |
|---|---|---|---|
| 1 | **W01** 继续/恢复仍遵守完整原始指令（TaskRecord 当前指令身份＋InputEnvelope 引用；2,000 字符只用于展示） | 继续任务时指令尾部约束不再丢失 | **已关闭（2026-09-10）：CI run `34397568867` 七 job 全绿确认**（`38b133a`）：TaskDirective 保留 sealed input 引用/inline body，继续先过 durability gate 再按原 run 认证读取；legacy 恰在旧上限拒绝；checkpoint 验证；agent-replay 不再用 preview 冒充正文。回归 `tests/turn/directive.rs` 4 项；agent-runtime turn 119＋lib 373＋actor 74、agent-replay 59、host_restore 3、fmt、clippy 全绿 |
| 2 | **W04+W08** 长维护可取消、失败后原文仍在（维护预算＋取消身份；summary_completed/unavailable 区分） | 132 次串行压缩调用变为有界可取消；模型失败不再把 fallback 当成功退役正文 | **已落地（2026-09-10，本批）**：RollingConfig 调用数预算（默认 4）＋`deferred_folds` 如实延期；BeforeModel 维护改为 spawned 可取消 op（abort=引擎安全失败，FoldRestore 归还记录），cancel_turn 阻塞维护中拿到类型化回执；compactor 错误/空回复传播为 Err，源正文不退役。回归：预算/延期/收敛（context-baselines）、门控取消（turn/maintenance.rs）、失败保留源（agent-compose 引擎级） |
| 3 | **W02** 最终 packing 复用 R09 范围覆盖（required 移除即 miss） | 缺必需正文时不会被告知证据齐备 | **已落地（2026-09-10，本批）**：`record_final_pack_drop` 改用 `visible_body_windows_cover`——互补区间删除即 required miss、整文副本/相同正文不误报、partial 不构成覆盖。回归含报告反例 |
| 4 | **W03** 全部 Storage GC 删除入口共享保留根（含根集合完整性） | 保留的旧快照持续有可恢复正文 | **已落地（2026-09-10，本批）**：`storage_gc_protecting(roots, complete)`＋`reconcile_store_protecting` 同签名扩展；根枚举失败/截断置 incomplete → 删除分支延期并报告；完成边界 GC 经 `context_storage_gc_protecting` 传入保留根。回归：引擎级完整序列（保护存活/延期可见/对照删除） |
| 5 | **W05** 当前验收证明优先保留（9 域合法任务可收敛） | 合法多域验收能结束，重复检查不挤掉必要证明 | **已落地（2026-09-10，本批）**：`MAX_VERIFICATION_FACTS` 对齐契约 16 域；cap 淘汰改为「每 identity 保留最新一条」，同域重复不再挤掉其他域；basis 变更失效规则不变。回归：9 域全保留＋重复风暴＋spec 变更失效 |
| 6 | **W06/W07** 两个小切片：artifact.read 大工件可达读取；patch 纠错候选取真实磁盘内容 | 大输出能按需查看；纠错依据真实内容 | **已落地（2026-09-10，本批）**：artifact.read 改流式按行扫描（8 MiB 扫描预算＋2 MiB 捕获上限，`total_lines_complete`/`window_truncated` 诚实标记，3 MB 工件首页与第 25,000 行均可达）；patch 失败候选取磁盘原文＋失败 hunk 序号，永不引用未提交中间态 |
| 7 | 三类真实仓库任务衡量（跨模块修改/多域验收/长输出＋中断恢复） | 成本与交互性有实测记录 | **PARTIAL（2026-09-11）**：三个固定小型真实代码样本已运行，产物 3/3 通过，见本节首轮基线回执。大型跨 crate、严格九份独立 host 证明、真实 compactor 长等待和旧版同任务对照未验收。dynamic ingest 取消已通过本地门控验收；本行后续并入「可靠长任务工作台」的统一用户旅程，沿原编号推进。密钥仅从进程输入注入；不为此新建评测框架或总门禁，不重开 M15/LT-EVAL |

**W 系列收口（2026-09-10）：**W01–W08 八项全部代码落地；**CI 确认：run `34408215832` 七 job 全绿**（覆盖 `8a0dc29`，含 W01 全树）。第 7 行仍属部分覆盖，不能由原 CI 或合成缓存结果补齐；后续沿本阶段统一旅程推进，不另立项。缺陷明细与逐项验收记录见 [AUDIT_TODO.md](AUDIT_TODO.md) 2026-09-10 表。

**用户结果：**继续不丢指令、维护可取消且失败不退役正文、证据缺失诚实可见、旧快照可恢复、合法验收能收敛、大输出可查看、纠错有真实依据。

---

## 当前队列（M17 收尾：N 系列——已全部收口，保留为关闭记录）

**说明（2026-09-11）：**N0–N8 全部关闭（关闭与 CI 确认记录见本表各行与 [CURRENT.md](CURRENT.md)）。下表作为关闭证据保留，不再是当前执行队列；历史批次入口顺序记录在其后「并行与进入顺序」。

| 顺序 | 工单 | 线 | 交付物 | 状态 | 依赖 |
|---|---|---|---|---|---|
| 1 | ~~N0~~ | 集成 | fmt/cfg 修复＋宿主 Linux 构建＋.NET 入 CI＋测试修准 | 已关闭（2026-09-08）：CI run `34163939549` 七 job 全绿；另修 conformance 角色准入、protocol 夹具 lint、replay 探针夹具、supervision 锁退避 | 无 |
| 2 | ~~N1~~ | 平台 | 宿主多连接、会话释放、端点所有权、可靠停机 | 已关闭（2026-09-08）：连接归属 grant＋revoke、65 次重连回归、有界停机、fail-closed UDS；CI run `34173331100` 全绿 | N0 ✓ |
| 3 | ~~N2~~ | 客户端 | 未知修改不重发、连接终态不复活、single-flight | 已关闭（2026-09-08）：修改只发一次（Unknown 语义）、终态故障路径＋半帧毒化、single-flight＋代际、双沿验证；dotnet 35/35 | N0 ✓ |
| 4 | ~~N3~~ | 契约 | 事件 receiver 保留到连接、notification 验证、多行正文 | 已关闭（2026-09-08）：契约放行多行＋字节上限（`76ef359`）；宿主保留 receiver＋watermark 切点＋同步管道轮询修复（`43a198b`/`184ace4`）；客户端按 kind 分派＋有界事件流（C3 两提交）；CI run `34268863699` 全绿 | N1 ✓, N2 ✓ |
| 5 | N4 | GUI | 计划/输出/知情审批/取消/继续/真实状态 | **主体已落地**（2026-09-09，`9fb2030`/`433d21e`/`843803f`）：知情审批快照（gate 风险＋有界目标摘要）＋桌面稳定行＋真实事件消费＋真实宿主默认传输；dotnet 56/56（提交记录）；**已关闭（2026-09-08/09）：CI run `34278810636` 七 job 全绿**（=三线 C1/C2 主体） | N3 ✓ |
| 6 | ~~N5~~ | 平台/GUI | 正式信封恢复＋结果/差异/工件按需读取 | **已关闭**：恢复半（2026-09-08，`808773c`，CI `34268863699`）；Rolling focus 跟踪 backlog 由 A3 关闭（ROLLING-FOCUS，host_restore 用默认 Rolling profile 验证）；结果/差异/工件/只读 Context 读取由 B3 四条只读路由＋C3 GUI 接线关闭（CI `34355401858`） | N2 ✓, N3 ✓ |
| 7 | ~~N6~~ | 基础 | 决策不误终结、lease 跨层一致、Skill 受限句柄、catalog 有界 | 已关闭（2026-09-08）：F15 决策需证明、F16 跨层到期保护、F17 包内普通文件围栏、F18 惰性投影；context-simple 302/302 | N0 ✓ |
| 8 | ~~N7~~ | GUI/测量 | 对象与文本保留有界、指标覆盖如实 | **已关闭（2026-09-09，`87850ae`＋C4 记录）**：MetricsSession 覆盖标注 root_only/full_tree/unknown（Windows 不冒充 whole-tree）＋有界采样环＋显式 idle 标记＋DeltaCoalescer 定时刷新（短 delta 后无输入也按间隔刷新）；输出行/字节双界（N4 已落）＋ViewModel 关闭释放 coalescer；MetricsSession/DeltaCoalescer 测试 9/9、dotnet 全量 72/72、桌面构建 0 错误 | N4 |
| 9 | ~~N8~~ | 扩展/交付 | MCP/Plugin 可配置使用＋Rust/.NET 来源绑定发布（并入 PACKAGE-01、原 R1） | **已关闭（2026-09-09）**：PACKAGE-01 来源绑定打包（`3352273`：--target-dir 构建与复制同身份、干净 staging、agent-host/desktop 入包、SOURCE.txt、递归 SHA256SUMS、PS 原生退出码；Windows 端到端验证通过，HEAD 重验 SOURCE SHA 绑定一致）；.NET→宿主→Runtime→工具→事件→GUI 全链 e2e（`aeddfbd` HostChainTests，真实宿主二进制＋demo model）；B4 宿主受限 MCP/Plugin 配置（`0515efb`：--mcp-config/--plugins-root bounded deny-unknown fail-closed、compose 接线、supported/unsupported 声明；agent-host lib 7/7、host_config 3/3、e2e 8/8 回归、restore 3/3）。Linux dist 侧由 CI package job 复核 | N1–N6 |
| 10 | ~~PACKAGE-01~~ | 条件 | 打包来源绑定 | **已并入 N8 关闭（2026-09-09，`3352273`）**：Windows 端到端打包验证通过；Linux 侧由 CI package job 复核 | — |
| 11 | MCP-01 | 条件 | MCP 写/连接/读可取消 | E1 已覆盖声明车道；新声明路径触发时补 | — |

阶段后候选（不作为本阶段前置）：只读工具子 Agent（独立状态、有限预算、无递归）；Context/GC/搜索的算法优化并入 A4，按真实瓶颈验收，不单独立项、不阻塞 B/C。

---

## 并行三线（2026-09-09 审查 `bbf7f5d`；与 N 系列同步执行、互不干扰）

来源：2026-09-09 外部三线审查（基线 `bbf7f5d3080747fe113a4a07469ac2fd4ccf2d35`；静态审查＋远端 CI 观察＋隔离探针，无完整 checkout、无工具链，20 crate 目录树核对、28 路径重点阅读）——[reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)。缺陷明细与「不要做什么」见 [AUDIT_TODO.md](AUDIT_TODO.md) 的 2026-09-09 表；标注「已复核」的定位已于 2026-09-09 在本工作树 HEAD `7e026ee` 静态确认。

**执行关系：N 系列主顺序不变（N4 收尾验收 → N7 → N8）；三线切片与之并行推进，互不阻塞。重叠切片一次执行、双方同时关闭，不重复立项、不拆第二套待办。**

**所有权与不干扰规则：**
- **A 线**拥有 `context-simple`、`context-baselines`；B/C 不改这两个 crate 的语义路径。
- **B 线**拥有 `agent-host`、`agent-platform-protocol`、`clients/dotnet` 与共享 DTO；`agent-contracts`/`RuntimeCommand`/compose 入口仍单一维护者（B 线合入），A 提证据/恢复字段需求、C 提界面实际需求，不各自发明 DTO。
- **C 线**拥有 `apps/Agent.Desktop` 界面与 ViewModel 生命周期。
- 每线内部按切片串行（A1→A2→A3→A4 等）；三线不在同一文件上互相覆盖，跨线需求走接口请求。

### A — 上下文、GC、搜索

| 切片 | 交付 | 对应缺陷（AUDIT_TODO 2026-09-09 表） | 与 N 系列关系 |
|---|---|---|---|
| A1 | **已落地（`6f47a90` 修复＋`df5b972` 测试，随 N4 批次合入）**：GC recall 不先删 blob（持久归属）、GC 外置取消安全、隔离失败不丢 owner、拒绝准入无副作用 | GC-DEL / GC-CANCEL / QUARANTINE / ADMIT-TERMINAL —— 均已落地 | 附带 ADMIT-TEST 确定性屏障同批落地（`6f47a90`，替代比例断言）；context-simple 全量绿 |
| A2 | **已落地（2026-09-09）**：截断后行范围/partial/required 传播一致、最终曝光如实 | RANGE-PARTIAL —— 已落地 | — |
| A3 | **已落地（2026-09-09）**：Rolling 折叠输入包含旧摘要、默认 profile 恢复（focus 跟踪）、显式准入使用期 | ROLLING-PRIOR / ROLLING-FOCUS / ADMIT-LEASE —— 均已落地 | ROLLING-FOCUS 即 N5 backlog 项（Rolling 不跟踪 focus）——代码缺口已关闭，正式冷恢复声明仍等 N5 验收 |
| A4 | 查询预处理复用、候选相关性进最终排序、边际预算装配、维护成本预算 | 报告第七节设计建议，非缺陷 | 算法优化按真实瓶颈验收，不阻塞 B/C |

**用户结果：**长期任务中的旧线索能找回；第二次压缩不会无意抹掉前次摘要；GC 不把 RAM 迁移误当持久保存。

### B — Runtime 与平台一致性

| 切片 | 交付 | 对应缺陷 | 与 N 系列关系 |
|---|---|---|---|
| B1 | 快照/订阅同一切点、live 与 durable 分流、重连代际/epoch 边界 | SNAP-GAP / LIVE-DELTA（两项均**已落地 2026-09-09**，见 AUDIT_TODO 注记） | N3 已接通链路，B1 修一致性残余——**已关闭**（agent-host 单测＋e2e 7/7、agent-contracts 163、dotnet 客户端测试全绿；无 wire 变更） |
| B2 | 队列 Completion 语义、宿主「取消全部」与「确认结束」分离、服务失败收口、retyped 原始信封验证 | QUEUE-COMPLETION / CANCEL-ALL / RETYPED（三项均**已落地 2026-09-09**，见 AUDIT_TODO 注记；agent-host 单测 5＋e2e 8/8、dotnet 72/72——计数含并行线当日新增） | N1 可靠停机的收口延伸——**已关闭（2026-09-09）：CI run `34355401858` 七 job 全绿确认**（该 run 覆盖 B3 校验器 clippy `380b08a` 修复后的 B 线全树） |
| B3 | GUI 所需真实任务/审批详情/结果/工件/只读 Context 接口 | — | 即 N5 结果半（结果/差异/工件按需读取）——**已关闭（2026-09-09）：CI run `34355401858` 七 job 全绿确认**：4 个 run-scoped 只读路由 `work.task_detail`/`work.changes`/`work.artifact`/`work.context`（协议 DTO＋验证、workspace `read_changes` 有界尾读、Runtime `TaskDetail` 命令、宿主 dispatch/routing、.NET DTO＋AgentConnection 4 方法）；protocol 42/42、workspace 105/105、runtime actor 72/72、host 5＋e2e 8/8＋restore 3/3、dotnet 77/77 |
| B4 | 同一正式宿主 profile 多入口复用、已有 MCP/Skill 配置接入 | — | 即 N8——**已关闭（2026-09-09）：CI run `34355401858` 七 job 全绿确认**：宿主 `--mcp-config`/`--plugins-root`（`agent_host::config`）bounded/deny-unknown/fail-closed 解析＋compose 接线＋supported/unsupported 启动声明；agent-host lib 7/7（含 config 2 项）、host_config 3/3、e2e 8/8 回归、restore 3/3、smoke 三条失败路径均退出 1 |

**用户结果：**重连不漏中间状态；坏连接与退出有明确结局；多入口调用同一套应用行为。

### C — 正式原生桌面与产品交付

| 切片 | 交付 | 对应缺陷 | 与 N 系列关系 |
|---|---|---|---|
| C1 | 真实事件消费者、模型/工具输出、真实计划与状态 | GUI-EVENTS（基线时点；`843803f` 已落地，随 N4 验收） | 即 N4 主体 |
| C2 | 知情审批、稳定行对象、单次刷新、连接代际与关闭清理 | F12/F13（`9fb2030`/`433d21e`/`843803f` 已落地，随 N4 验收） | 即 N4 主体＋N7 前半 |
| C3 | 修改审阅、正式冷恢复走查、工件按需读取、只读 Context 检查 | — | 即 N5 结果半（GUI 侧）。**已关闭（2026-09-09）：`391f121` 未知提交按快照事实解除＋restore-walkthrough 走查测试；`93c300d` B3 四个只读路由（task_detail/changes/artifact/context）；`17409f1` C3 GUI 接线骨架（审阅 Tab：任务详情锚点卡/变更日志/工件＋Context 面板真实列表，全部只读、诚实 unavailable、era 丢弃、断开清空）；`a1033fe` 真实 host 链审阅钻取（四条 B3 路由经真实宿主＋工作台完整走查，dotnet 87/87）。真实 provider 与真实写变更场景照旧 NOT_RUN** |
| C4 | 长会话资源上界、准确测量、Rust＋.NET 来源绑定包 | F14 | 即 N7＋N8。**已关闭（2026-09-09）：N7 `87850ae`（MetricsSession 覆盖标注/有界采样环/显式 idle/时间预算 coalescer）＋N8 整链（`3352273` 来源绑定打包、`aeddfbd` 全链 e2e、`0515efb` B4 宿主 MCP/Plugin 配置；Windows dist 端到端打包在 HEAD `149f06c` 重验：SOURCE SHA 绑定一致、desktop 子目录＋递归 SHA256SUMS 完整、dotnet 87/87）** |

**用户结果：**正式客户端能提交、观察、审批、继续、恢复和审阅，不依赖布局夹具。

### 首批与依赖

- **A1–A3 已落地**（A1：`6f47a90`＋`df5b972`，随 N4 批次合入；A2/A3：2026-09-09 本批提交，context-simple 311＋context-baselines 11 全绿，host_restore 在 A 提交树 3/3）；B1/B2 已关闭。A4 为报告第七节设计建议（非缺陷，按真实瓶颈验收，不阻塞 B/C）；C1/C2 主体已随 N4 落地，其剩余（真实计划/open-loops 投影，当前诚实显示「不可用」）依赖 B3 的快照字段；C3/C4 分别接 N5 结果半与 N7/N8。
- C 的事件消费与对象生命周期不等 A 的算法实验；但**冷恢复、证据完整性等正式支持声明，必须等对应 A/B 回归通过**（沿用既有 B1/B2 声明门槛原则）。
- 每个切片回执照旧：改了什么、接进哪个真实用户动作、实际跑了什么、还有什么没验证、下一步是什么。

---

## W 系列明细（2026-09-10 审查 `fb1ec9c`：Agent 任务流程核心——代码全部落地，第 7 行仍有部分覆盖）

**实施后复核（2026-09-10，`8a0dc29` → `6ec044a`）：**下表保留原实施与 CI 回执，新增反例仍沿原编号执行，不另建阶段。详见 [复核与缓存设计](reviews/2026-09-10-cache-design-8a0dc29/REPORT.md)、[验证记录](reviews/2026-09-10-cache-design-8a0dc29/EVIDENCE.md)。

- **W02：已修复并包含于 `6ec044a`**。候选必须具有自己的文件/版本与完整覆盖范围；同 id 不同片段、跨来源同文本不能掩盖 required_miss。
- **W04 非 BeforeModel 取消已落地（2026-09-10，工作树）：**UserInput/AfterTool/AfterModel 维护接入既有 operation 续接，取消先推进 Core 代际再停止并 join 维护。新输入恢复入账前快照；提交阶段中断返回 RecoveryRequired，保留已应用效果，不误发 TurnCompleted。输入事务成功后使用新 directive 的执行/证明版本；停机有界等待当前提交。门控与完成/取消竞争回归见 [W04 实施与验证回执](reviews/2026-09-10-maintenance-cancellation.md)。W04(P2) 的零预算延期账目已在 `732cf93` 修复，本片不重做。
- **W06 已包含于 `732cf93`：**按渲染字节统计捕获上限，截断保留行边界，游标指向首个未展示行；本片核对 HEAD 后跳过重做。**第 7 行已完成小型基线，范围与下一片见下条**；不复跑缓存合成样本。
- **W04 dynamic ingest 已完成本地验收（2026-09-11，工作树）：**取得真实 Simple 引擎＋门控 compactor 的修复前取消超时反例后，将 ingest 与 UserInput 维护纳入同一 operation/输入事务。取消和停止先 join 后恢复完整快照，继续保留原指令；正常压缩入账/用量报告各一次；部分输入失败回滚，回滚失败返回 RecoveryRequired。Runtime 620 项及 Clippy 通过，[本片回执](reviews/2026-09-11-ingest-cancellation.md)。Flash 原样本没有触发压缩，不充当关闭证据。**下一片先准备第 7 行的严格九份独立 host 验证覆盖声明任务，固定起点、目标与产物检查后再运行有界 Flash；不重复成功样本。**
- **第 7 行首轮基线已执行，部分覆盖：**三个小型隔离真实代码样本产物通过；已知 21 次 usage、另 1 取消操作缺测。跨进程恢复和约 4 MB 工件尾部读取通过，九类应用检查不冒充九份独立 host 证明；大型跨 crate、真实 compactor 和降本对照仍未验收。[实测回执](reviews/2026-09-11-flash-workflow/REPORT.md)。
- **供应商 KV 缓存首切片（用户明确共同底层优先）：**按 [KV_CACHE_PLAN.md](reviews/2026-09-10-cache-design-8a0dc29/KV_CACHE_PLAN.md) 在 PromptAssembler 建立稳定段/current_view 的共同布局，稳定合法工具集合的表示、状态后置，同步记录前缀变化原因；随后有界区段，再由薄 provider 适配层映射专属参数与用量。底层不等待供应商选择；独立观测不被 W04/W06 整体阻塞，涉及提交边界才依赖其修复。不启用工具 memo、不改 GC/打分，不以离线字节前缀冒充 provider 命中率。

  **首个保守切片已落地（2026-09-10，工作树）：**`CurrentStateLast` 移动完整目录/焦点/进度块，保留全部文字、角色、选中顺序、正文去重、工具选择和协议窗口；新增布局版本及 Legacy 组合回退，request metadata 标明布局。[实施回执](reviews/2026-09-10-cache-design-8a0dc29/KV_LAYOUT_IMPLEMENTATION.md)。为保留当前焦点策略，本片不拆正文标题、不冻结选中集合、不稳定化实际已变化的 schema surface。下一步先做最终请求前缀变化归因和用量覆盖，再决定剩余布局改动；区段化与专属缓存参数尚未实施。

  **发送观测＋隔离复测已完成：**继[首次 10 次实测](reviews/2026-09-10-cache-live/REPORT.md)后，按需 provider 观测入口和 [12 次隔离预热/换序对照](reviews/2026-09-10-cache-live/ISOLATED_REPORT.md)已运行。当前 `eval.env` 可用，不再以旧“无凭据”回执作为阻塞。新布局超过 20KB 的 HTTP 前缀保持不变，但更新焦点/进度的两次请求仍为零读取；原样重放 5/6 命中，12/12 合成回答正确。由此进入下一段的共同复用边界实现。隐藏服务原因、金额和整仓任务质量未验证；不重复相同条件的付费调用，不重开长任务或冻结实验。

  **共同复用边界＋显式映射本地实现完成（2026-09-10）：**`ModelInput::into_request` 从最终 packing 的请求绑定单个 `PromptReuseBoundary`，实际正文/角色/顺序/schema 失配即失效；只在 `OPENAI_PROMPT_CACHE_MODE=responses_explicit` 且固定 Responses 协议时映射断点，默认供应商请求及 profile digest 保持。定向测试与 Clippy 通过。[本片回执](reviews/2026-09-10-cache-live/BOUNDARY.md)。**真实验收未过：2 次冷请求失败，第二次 HTTP 400 且错误提到 `prompt_cache_breakpoint`；未进入改 D 复用 E 阶段。后续小请求已查明服务端明确报告当前模型不支持该断点，见 [能力定位与类型化诊断](reviews/2026-09-10-cache-live/CAPABILITY.md)。当前默认模式保持，显式收益验收需要已确认支持的端点/模型；不重复该拒绝请求，不改焦点角色/GC/选择策略。**

  **DeepSeek Flash 合成复用验收通过（2026-09-10）：**用户指定官方 `deepseek-flash`，以 Responses＋非思考档位＋原生默认缓存完成 10 次真实请求，答案 10/10 正确。三次当前状态变化，新布局每次缓存读取 6,144/6,468（94.99%），Legacy 256/6,468（3.96%）。类型化推理档位配置与验证已落地；[实测回执](reviews/2026-09-10-cache-live/DEEPSEEK_FLASH.md)。本片完成；后续 W04 非 BeforeModel 维护取消现已在工作树接通，见上方回执。真实仓库质量/思考模式工具续跑/实际账单仍未验收，不重复本组合成实测。

来源：2026-09-10 Agent 任务流程审查（基线 `fb1ec9c`；主审查者逐项复核＋隔离反例，8 项：4 P1、4 P2）——[reviews/2026-09-10-agent-workflow-fb1ec9c/REPORT.md](reviews/2026-09-10-agent-workflow-fb1ec9c/REPORT.md)，缺陷明细与「不要做什么」见 [AUDIT_TODO.md](AUDIT_TODO.md) 2026-09-10 表，切片顺序与衡量方式见 [WORKFLOW_AND_ROUTE.md](reviews/2026-09-10-agent-workflow-fb1ec9c/WORKFLOW_AND_ROUTE.md)。W 编号只是本轮定位，不另建阶段；此前 N/A/B/C/R 队列全部收口（CI 确认记录见 CURRENT.md）。

| 建议顺序 | 切片 | 用户能获得什么 | 状态 |
|---|---|---|---|
| 1 | **W01** 继续/恢复仍遵守完整原始指令（TaskRecord 当前指令身份＋InputEnvelope 引用；2,000 字符只用于展示） | 继续任务时指令尾部约束不再丢失 | **已关闭（2026-09-10）：CI run `34397568867` 七 job 全绿确认**（`38b133a`）：TaskDirective 保留 sealed input 引用/inline body，继续先过 durability gate 再按原 run 认证读取；legacy 恰在旧上限拒绝；checkpoint 验证；agent-replay 不再用 preview 冒充正文。回归 `tests/turn/directive.rs` 4 项；agent-runtime turn 119＋lib 373＋actor 74、agent-replay 59、host_restore 3、fmt、clippy 全绿 |
| 2 | **W04+W08** 长维护可取消、失败后原文仍在（维护预算＋取消身份；summary_completed/unavailable 区分） | 132 次串行压缩调用变为有界可取消；模型失败不再把 fallback 当成功退役正文 | **已落地（2026-09-10，本批）**：RollingConfig 调用数预算（默认 4）＋`deferred_folds` 如实延期；BeforeModel 维护改为 spawned 可取消 op（abort=引擎安全失败，FoldRestore 归还记录），cancel_turn 阻塞维护中拿到类型化回执；compactor 错误/空回复传播为 Err，源正文不退役。回归：预算/延期/收敛（context-baselines）、门控取消（turn/maintenance.rs）、失败保留源（agent-compose 引擎级） |
| 3 | **W02** 最终 packing 复用 R09 范围覆盖（required 移除即 miss） | 缺必需正文时不会被告知证据齐备 | **已落地（2026-09-10，本批）**：`record_final_pack_drop` 改用 `visible_body_windows_cover`——互补区间删除即 required miss、整文副本/相同正文不误报、partial 不构成覆盖。回归含报告反例 |
| 4 | **W03** 全部 Storage GC 删除入口共享保留根（含根集合完整性） | 保留的旧快照持续有可恢复正文 | **已落地（2026-09-10，本批）**：`storage_gc_protecting(roots, complete)`＋`reconcile_store_protecting` 同签名扩展；根枚举失败/截断置 incomplete → 删除分支延期并报告；完成边界 GC 经 `context_storage_gc_protecting` 传入保留根。回归：引擎级完整序列（保护存活/延期可见/对照删除） |
| 5 | **W05** 当前验收证明优先保留（9 域合法任务可收敛） | 合法多域验收能结束，重复检查不挤掉必要证明 | **已落地（2026-09-10，本批）**：`MAX_VERIFICATION_FACTS` 对齐契约 16 域；cap 淘汰改为「每 identity 保留最新一条」，同域重复不再挤掉其他域；basis 变更失效规则不变。回归：9 域全保留＋重复风暴＋spec 变更失效 |
| 6 | **W06/W07** 两个小切片：artifact.read 大工件可达读取；patch 纠错候选取真实磁盘内容 | 大输出能按需查看；纠错依据真实内容 | **已落地（2026-09-10，本批）**：artifact.read 改流式按行扫描（8 MiB 扫描预算＋2 MiB 捕获上限，`total_lines_complete`/`window_truncated` 诚实标记，3 MB 工件首页与第 25,000 行均可达）；patch 失败候选取磁盘原文＋失败 hunk 序号，永不引用未提交中间态 |
| 7 | 三类真实仓库任务衡量（跨模块修改/多域验收/长输出＋中断恢复） | 成本与交互性有实测记录 | **PARTIAL（2026-09-11）**：三个固定小型真实代码样本已运行，产物 3/3 通过，见本节首轮基线回执。大型跨 crate、严格九份独立 host 证明、真实 compactor 长等待和旧版同任务对照未验收。dynamic ingest 取消已通过本地门控验收；下一片先准备严格九份 host 声明的实际任务。密钥仅从进程输入注入。衡量的计数面已就位——决策调用走 `RuntimeEvent::ModelUsed`（input/output/attempt/retiy），维护调用走 `ContextMaintenanceReport` 的 `compaction_input_tokens`/`compaction_output_tokens`/`compactions[]` 与 `deferred_folds`，交互性走 TurnCancelled/审批回执时延与 MetricsSession，有界性走 `deferred_folds`/工件读取字节上限。执行前置：① provider 凭据注入宿主 profile（不打进仓库）；② 固定三类任务的仓库起点/目标/验收/模型配置各一份；③ 按路线文档第五节记录总成本（决策/维护分列）、无进展动作、取消时延、交付正确性、有界性五组数据。**不为此新建评测框架或总门禁，不重开 M15/LT-EVAL** |

**W 系列收口（2026-09-10）：**W01–W08 八项全部代码落地；**CI 确认：run `34408215832` 七 job 全绿**（覆盖 `8a0dc29`，含 W01 全树）。第 7 行已完成首轮小型样本，仍属部分覆盖，不能由原 CI 或合成缓存结果补齐；ingest 边界已本地验收，后续沿第 7 行未验覆盖推进，不另立项。缺陷明细与逐项验收记录见 [AUDIT_TODO.md](AUDIT_TODO.md) 2026-09-10 表。

**用户结果：**继续不丢指令、维护可取消且失败不退役正文、证据缺失诚实可见、旧快照可恢复、合法验收能收敛、大输出可查看、纠错有真实依据。

---

## N0 — 恢复构建与验证入口（已关闭 2026-09-08，CI run `34163939549` 全绿）

**用户结果：** 支持平台（Linux/Windows）的宿主与客户端重新可被 CI 真实验证；测试名与实际验证路径一致。

**事实：** CI run `34148921895` 在两个平台均停于 `cargo fmt --check`（违规集中在 `crates/agent-host`）；`HostServer::serve` 的 NamedPipe 分支无条件引用 `#[cfg(windows)]` 的 `winpipe` 模块（`lib.rs:234` vs `:503`），Linux 宿主构建存在静态缺口；host_e2e 的 Unix 连接辅助无真实 UDS。

**步骤：** `cargo fmt --all`（仅必要范围）；为两个平台提供明确 cfg 分支或受支持/不支持实现；现有 CI 增补 agent-host Linux 分片与 .NET build/test；修准 F20 列出的现有测试（半帧样本应为合法长度前缀、同连接乱序、原 key 重试、服务线程错误必须检查）。

**检查：** `cargo fmt --all -- --check`；`cargo check -p agent-host --all-targets --locked`（Linux＋Windows）；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`；`dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`。

**做到这里停止：** 不新建评测框架；不等全仓人工复测完成才开 N1/N2/N6。

## N1 — 宿主可长期接入、可安全关闭

**用户结果：** 第二个客户端能连入；反复重连不耗尽会话表；Ctrl-C 有界退出；端点不被误删/误抢。

**已复核事实：** `winpipe.rs:186` 每个管道实例都带 `FILE_FLAG_FIRST_PIPE_INSTANCE`（第二实例创建失败→accept 退出）；连接退出只 drop router 不 revoke session（64 上限 `.expect`，第 65 次顺序连接 panic）；accept 循环无停止通道（Ctrl-C 后 join 可能永久阻塞）；`lib.rs:187/251` bind 前无条件 `remove_file`（可删普通文件/解除他人 listener；默认 `/tmp` 固定名跨工作区冲突）。

**步骤：** 首实例独占语义仅用于名称占用检查，后续实例正常模式＋RAII 句柄；session grant 归属连接 guard，全部退出路径 revoke，install 错误受控拒绝；显式停止信号＋连接集合关闭＋服务失败回执，有界 join；UDS 用户私有、按工作区区分的端点，只清理可证明属于自己的。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；两并行客户端＋≥65 次顺序重连授权表回落；无客户端/半帧/慢读客户端下有界停机；普通文件占端点拒绝且不删除。

**做到这里停止：** 不做公网 TCP/HTTP、系统服务安装器、多工作区调度。

## N2 — 未知修改结果不重发，连接状态不复活

**用户结果：** continue 已被接受但回复丢失时不会自动续跑第二段；坏连接不被复用；并发连接请求只建一条连接。

**已复核事实：** `ResumableSession.RunAsync` 捕获连接异常后重连并重试 operation，submit/continue/cancel 全走它（审批答复已正确排除）；continue/cancel 正文不绑定预期 task/turn/generation；`Fault()` 只失败 pending 不关流不标终态；半帧写入失败不毒化连接；`LiveAsync` 连接/握手在锁外、无 single-flight 与代际约束。

**步骤：** 查询与修改重试策略分离，未知修改结果返回 Unknown 并要求查询/重同步；修改携带预期 task/turn/generation＋宿主 incarnation；single-flight connect＋handshake 成功才安装＋Dispose 防迟到复活；Fault 单一终态路径（关流、拒新请求、结清 waiter）；每个类型化 API 在实际发送/接受路径运行 payload 验证（F19）。

**检查（建议，未执行）：** `dotnet test …Agent.Client.Tests.csproj --filter "ConnectionTests|ResumableSessionTests"`；`cargo test -p agent-platform-protocol work`。

**做到这里停止：** 不建通用持久幂等数据库；无法证明时返回 Unknown，不猜测。

## N3 — 同一连接上的事件、快照和完整输入

**用户结果：** 订阅后真实收到受理/工具/助手/终态事件；多行开发要求可提交。

**已复核事实：** `agent-host/src/lib.rs:425` `work.subscribe` 握手成功后 `Ok((response, _receiver))` 丢弃事件 receiver；客户端 `Dispatch` 在分型前要求 `request_id`（合法 notification 被拒）；`work.rs:465` `validate_text` 拒绝一切控制字符（含 LF/TAB）而 GUI 文本框 AcceptsReturn=true。

**步骤：** 宿主保留 receiver 到连接关闭，响应/通知共用单一有界 writer；客户端按 kind 验证（notification 无需 request_id）；snapshot+subscribe 一致切点（沿用 barrier/resync-only，缺口返回 resync_required）；短标题与有界完整正文分离，正文允许合法换行/制表，身份/路径仍严格。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；`cargo test -p agent-platform-protocol work`；`dotnet test …Agent.Client.Tests.csproj`；中文多行/emoji 提交、慢消费者触发明确 gap/resync。

**做到这里停止：** 不新建 Chronicle；不为跨语言更换长度前缀传输。

## N4 — 从按钮和快照变成真正的任务操作面（当前工单：主体已落地，验收待 CI）

**用户结果：** 提交→工具/输出→知情审批→让出/取消→继续→产出待审是一条正式链；待审批反复刷新不积累命令对象。

**事实（审查基线 `bbf7f5d` 时点）：** 待审批快照只有 request_id＋call_name（无路径/argv/参数/风险）；每 3 秒刷新重建审批行、每行两个命令加入长寿命 `AsyncCommandGroup` 不移除（推导：1 小时≈2400 引用，非实测）；桌面默认 FixtureLayout。

**已落地（2026-09-09，`9fb2030`/`433d21e`/`843803f`，提交记录）：** ① 知情审批快照（F12）：`pending_approvals` 携带 gate 类型化风险（`ApprovalRisk`，wire snake_case）＋256 字符有界操作员目标摘要（投影自 path/files[].path/command/argv 等结构化参数，缺失为 None→UI 显示「不可用」，不从文字推断）；.NET 镜像 DTO fail-closed 解码（risk 缺失即拒收）＋共享端点推导 fixture。② 稳定行生命周期（F13）：审批行按 request_id 复用、移除撤销 `AsyncCommandGroup` 注册（200 次刷新演练计数恒定）、刷新 single-flight＋连接代际否决迟到结果、窗口关闭取消单一 lifetime。③ 真实事件消费（N3 API）：默认通道接真实宿主（fixture 降为显式「布局预览（非执行器）」）；单一后台消费者读 `IAgentConnection.Events`，UI 线程渲染类型化事实（模型增量过 DeltaCoalescer），3 秒轮询降为 10 秒兜底；输出 400 行＋64 KiB 双界限整行淘汰。④ 诚实状态：运行态仅由类型化快照布尔渲染；计划/open-loops 缺字段时显示「不可用」；未知提交结果保留 client_request_id 幂等重试，审批答复不自动重试。测试：WorkbenchLifecycleTests＋WorkbenchIntegrationTests（ScriptedEventHost 上 submit 回执→工具事件→审批详情→Delivered→终态），dotnet 56/56、桌面构建 0 错误（提交记录）。

**验收待确认：** CI run `34278244036`/`34278810636`（文档写作时进行中）；真实计划/open-loops/结果卡投影等 B3 快照字段（当前诚实显示「不可用」）；真实 provider 场景照旧 `NOT_RUN`。

**检查：** 已执行（提交记录）：`dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj` 56/56；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。CI 全量确认后关闭本工单（同时关闭三线 C1/C2 主体）。

**做到这里停止：** 不做 IDE/编辑器；GUI 不保存第二份任务/权限/完成真相。

## N5 — 正式检查点恢复与可审阅工件

**用户结果：** 同一正式宿主保存→关闭→重启→恢复原任务并显式继续；结果/差异/工件按需可读。

**已复核事实：** `agent-host/src/main.rs:207-213` `--restore-latest` 按文件名枚举 JSON 后直接 `serde_json::from_str::<RuntimeCheckpoint>`，绕过 CheckpointStore 的版本/checksum 信封（`decode_checkpoint_file/bytes` 已存在未用）。

**步骤：** 统一走 CheckpointStore 受限验证解码＋完整 `RuntimeInstance.restore`；profile 来源与身份显示明确；变更/工件经现有事实读取，只读调用不新建模型回合；GUI 区分断开窗口/取消任务/停止宿主/恢复继续。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；`cargo test -p agent-compose`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。

**做到这里停止：** 不改检查点格式、不建第二恢复引擎。

## N6 — 修准语义保留、只读成本与 Skill 路径

**用户结果：** 同文件两条兼容决策共存；有效 lease 不因正文位置改变终态；Skill 读取不能经 symlink/FIFO 越出包外；catalog limit=0 不做全量投影。

**已复核事实：** 决策 supersession 仍按实体/子串重合排队 `Superseded`（无同任务/决策键/显式替代约束）；`residency.rs:315-316` 的 Warm 路径检查 keep_alive/lease 而 Resident TTL 路径（`gc/minor.rs`）无此检查；`plugin.rs` `skill_read` 词法相对检查后普通 `File::open`（包内 symlink 可指向包外，探针已证机制；FIFO 可在 take 生效前阻塞）；`engine.rs:1705/1710` `to_summaries` 先全量投影再 `bounded_catalog`。

**步骤：** 实体匹配降为相关性，仅明确替代目标＋正确任务范围才进终态；提取跨层共用到期保护（lease/keep_alive 范围明确，终态不可复活）；catalog 早退 limit0＋惰性投影/选中后复制；Skill 复用既有 ConfinedDir/受限普通文件句柄。拆小 PR。

**检查（建议，未执行）：** `cargo test -p context-simple`；`cargo test -p agent-runtime --lib plugin::`。

**做到这里停止：** 不加向量/学习排序/新 GC 策略；可与 N1/N2 并行。

## N7 — 长会话有界而且指标可解释

**用户结果：** 长会话命令、pending、事件、文本、采样缓存有界回落；指标覆盖范围如实标注。

**事实：** MetricsSession Windows 无法枚举 parent 仍报 whole-tree、Linux 遍历提前标记 seen 可能漏孙进程、末样本直接标 idle、`_samples` 无限追加；DeltaCoalescer 仅 Append 时检查时间；输出 400 行限制不等于字节界限。

**步骤：** 输出按 chars/bytes/rows 共同限额、增量显示；采样覆盖标 root_only/full_tree/unknown、有界环；关闭时挂接 ViewModel 异步清理。

**检查（建议，未执行）：** `dotnet test …Agent.Client.Tests.csproj --filter "MetricsSessionTests|DeltaCoalescerTests"`；`dotnet build … -c Release`。

**做到这里停止：** 不把 AOT/零拷贝列为发布前置；不新增指标数据库。

## N8 — 把现有能力接入产品并完成来源绑定发布

**用户结果：** 安装后连接真实宿主而非 fixture；MCP/Skill 经目录与权限边界可配置使用；包来源可追溯。

**已落地（2026-09-09）：** 打包来源绑定（PACKAGE-01，`3352273`，随 N7 关闭确认）；.NET→Rust host→Runtime→Tool→Event→GUI 真实用例（`aeddfbd` 全链 e2e）；**B4 能力配置（B 线，本切片）**——宿主 `--mcp-config`（`agent_host::config::parse_mcp_config`：JSON 声明数组 bounded/deny-unknown/fail-closed，上限 32 服/256KiB，id 去重与语法校验，转 `McpServerDecl`）与 `--plugins-root`（`discover_plugin_packages`：子目录按名排序、`plugin.json` 缺失跳过、坏 manifest/静态准入失败即整体拒绝、复用以 `PluginPackageAdmission::validate_static`；main.rs 显式 root 即启用：install_from_root→enable→activate_skill）接线 compose；启动声明 supported（mcp_servers stdio/declared-permission risk；plugins install_from_root+skill_read）/unsupported（remote adapters、hook execution）。不默认启用 MCP（MCP-01 条件项未触发）。

**检查（本切片已执行）：** `cargo test -p agent-host --lib` 7/7（含 config 解析/发现 2 项）、`cargo test -p agent-host --test host_config` 3/3（坏声明 fail-closed、空配置 noop 回归、plugin root 经 compose 启用且 activation=Active）、回归 `cargo test -p agent-host --test host_e2e` 8/8 与 `--test host_restore` 3/3；smoke：`--mcp-config` 缺失/未知字段、`--plugins-root` 非目录均退出 1 并报错（`AGENT_DEMO=1` demo 模式、隔离 workdir）。未验证：真实 provider 与 MCP server 交互照旧 NOT_RUN（E1 已覆盖 mock 闭环）；Linux UDS 侧与 CI 全量待 run 记录。

**做到这里停止：** 不新建 release/tag 除非明确要求；本切片验收前不写 M17 已完成。

---

## M17 队列（主体落地，2026-09-07；闭环残余由上方 N 系列接手）

| 顺序 | 工单 | 线 | 交付物 | 状态 | 依赖 |
|---|---|---|---|---|---|
| 1 | C0 | 契约 | 阶段切换＋submit/continue/cancel/snapshot/subscribe/approval 最小 DTO 与语义 | **DTO 已落地（2026-09-07）**：`agent-platform-protocol/src/work.rs` 六条 run-scoped 路由＋事件通知 DTO＋金样序列测试；.NET 客户端同形镜像 | C0, P1 可开始 |
| 2 | B1 | 基础 | 监督身份、可靠台账、清理确认、宿主 proof 监督接线、watchdog 边界 | 主体已关闭（2026-09-07）：身份化台账/类型化对账门/清理回执/proof 接线/watchdog 组扫描，Windows＋WSL2 验证；扩展验收项随 P3 收口 | 可开始 |
| 3 | B2 | 基础 | metadata 发布不确定时的 writer 围栏 | 已关闭（2026-09-07）：`RecoveryRequired` 发布不确定＋compact 围栏＋注入测试；续审确认通过 | 可开始 |
| 4 | B3 | 基础 | Context 验证关联＋stdin/grant/输出接入边界 | 主体已关闭（2026-09-07）：同配方关联终结、stdin/grant 读取时计费、有界输出 sink；扩展范围（recipe 版本/覆盖身份关联、无期限 stdin 读取期限）见剩余表 | 可开始 |
| 5 | P1 | 平台 | 与 TUI 无关的原子工作提交与受理回执 | **已关闭（2026-09-07）**：`StartWork` 原子命令＋有界受理台账（同 id 同内容幂等、异内容拒绝）；TUI/无头共用 `agent_runtime::work`；F07 交叉投递 5 项验收测试全绿 | — |
| 6 | P2 | 平台 | 类型化快照、增量事件、审批与结果、重同步 | **主体已关闭（2026-09-07）**：runtime＋TUI 投影 revision 归属修复；`ToolFailureClass::ApprovalDenied` 类型化拒绝替代文字推断；`StatusSnapshot`＋watermark；`WorkControlRouter`（submit/continue/cancel/snapshot/subscribe/approval.respond＋会话授权）；事件重放窗口仍为 resync-only | — |
| 7 | P3 | 平台 | 正式 Rust 宿主＋Named Pipe/UDS 双向通信 | 已落地（2026-09-07）：`crates/agent-host` 编译＋命名管道 E2E 绿＋.NET 客户端互操作冒烟 exit 0；事件 wire 契约与 UDS 真机验证见 P3 节 | C0, P1, P2 |
| 8 | G1 | GUI | .NET 客户端库＋正式 Avalonia 外壳 | 已落地（2026-09-07）：`clients/dotnet/Agent.Client`＋`apps/Agent.Desktop`＋`global.json`(SDK 10.0.301)；C0 fixtures Rust/C# 双语一致测试绿；build/测试/启动冒烟通过 | C0 |
| 9 | G2 | GUI | 审批/取消/审阅/冷恢复完整链路 | 客户端与宿主链路已落地（2026-09-07）：重连不自动应答审批、断线/resync 横幅、宿主端审批经真实 gate 回执（E2E 绿）；差异按需读取与 GUI 端到端真实工具走查未做（等差异路由与真实 provider） | G1, P2, P3, B1, B2 |
| 10 | G3 | GUI | 长会话低开销工作台＋只读 Context 检查 | 客户端侧已落地（2026-09-07）：列表虚拟化＋有界保留（200 任务/400 行输出）、DeltaCoalescer、MetricsSession（未测写 NOT_RUN）、只读 Context 面板占位；首采样见 walkthroughs/2026-09-07-g3-desktop-metrics.md | G1, P2, B3 |
| 11 | E1 | 扩展 | 一个真实外围能力＋按需 Skill 最小闭环 | 已关闭（2026-09-07）：MCP 写/连接/读取消全贯通、compose 配置缝、真实 mock server 闭环集成测试、Skill 按需有界读取；Windows＋WSL2 验证 | P3, G1（闭环已先行落地） |
| 12 | R1 | 联合 | Rust＋.NET 来源绑定打包与正式使用收口 | 提案 | B1–B3, P3, G2, G3 |
| 13 | PACKAGE-01 | 条件 | 打包来源绑定 | 下次实际发布 | — |
| 14 | MCP-01 | 条件 | MCP 写/连接/读可取消 | 仅默认启用 MCP 时 | — |


## 上一阶段队列（已关闭，2026-09-06/07）

| 工单 | 交付物 | 状态 |
|---|---|---|
| ~~STORAGE-02~~ | 压缩已发布后失败则隔离旧 writer | 已关闭（2026-09-06）；2026-09-07 续审指出 helper 内 rename→目录同步残余 → B2 |
| ~~PROCESS-01~~ | 宿主验证硬崩溃监督 | 已关闭（2026-09-07）：Windows Job 围栏 + Unix 管道 EOF 看门狗 + 监督台账，全部在真 Linux（WSL2）验证；续审指出台账身份/确认残余 → B1 |
| ~~PROCESS-02~~ | reap 未确认退出不清 pid | 已关闭（2026-09-06） |
| ~~WORKSPACE-01~~ | 普通 open 不阻塞 FIFO | 已关闭（2026-09-06） |
| ~~WORKSPACE-02~~ | Windows 拒绝路径立即接管 HANDLE | 已关闭（2026-09-06） |
| ~~PROVIDER-01~~ | 错误 HTTP body 有界读取 | 已关闭（2026-09-06） |
| ~~PROVIDER-02~~ | Chat `length` 终止语义 | 已关闭（2026-09-06） |
| ~~PROVIDER-03~~ | Responses EOF 尾帧校验 | 已关闭（2026-09-06） |
| ~~CONTEXT-01~~ | 依赖候选 newest-first | 已关闭（2026-09-06） |
| ~~M16-02 剩余~~ | 待审阅 ≠ 持久完成 | 已关闭（2026-09-07） |

已关闭、跳过：STORAGE-01、DOC-01；EOF wait、消费 ACK、PromptRequired、resync。
关闭证据：STORAGE-02 `f9852ea`、PROCESS-01/02 `7c72df3`、WORKSPACE-01/02 `17c5ded`、PROVIDER-01/02/03 与 CONTEXT-01 `3e0128a`（定向测试计数见各提交说明）。
续审明确不再原样重报的旧问题：release 消费 ACK stamp、process.run 输出 EOF、Windows metadata 不再先 unlink、普通读取不再清错误、片段 supersession 覆盖判断——见 REPORT.md 第 4 节。

---

## C0 — 阶段契约（进行中：文档部分已落地）

**用户结果：** 不同入口使用同一操作、身份和快照语义；GUI 可直接进入长期实现。

**入口／拟新增路径：** `AGENTS.md`；`docs/CURRENT.md`、`docs/ROADMAP.md`、本文件（本次已改）；`crates/agent-platform-protocol/src/`（拟新增）。

**剩余步骤：**
1. 约定 submit/continue/cancel/snapshot/subscribe/approval response 的有限 DTO、身份、错误与大小上限；受理、应用、任务完成、清理确认分开。
2. 列清 supported/unsupported，不预定义全部未来 namespace。
3. 同一契约示例供 Rust/C# 使用；共享字段修改由单一负责人合入。

**检查：** `python scripts/doc_consistency.py`（本次已跑）；DTO 落地后加对示例的双语言含义一致测试。

**做到这里停止：** 不新建文档治理框架；不把"全部平台协议设计完"作为后续工单的开始条件。

## B1 — 监督身份、台账、宿主验证接线与有界清理

**用户结果：** 不凭旧 PID 误杀其它进程；未确认清理不丢监督记录；真实宿主 proof 路径应用与普通工具相同的监督策略。

**入口：** `crates/tool-runtime/src/supervision.rs`、`crates/tool-runtime/src/proof_runner.rs`、`crates/tool-runtime/src/tools/process.rs`、`crates/agent-process/src/watchdog.rs`、`crates/agent-process/src/lifecycle.rs`、`crates/agent-compose/src/lib.rs`。

**步骤：**
1. 复用 `lifecycle.rs` 的 `ProcessIdentity`、`inspect_process` 与 `terminate_matching_process_tree`；区分退出、身份不符、清理已确认与无法确认，观测错误不得成为退出证明；旧的无身份记录默认不得 kill（对齐 F01/F02）。
2. 台账记录/读取/结束返回 `Result`；限制大小与行数、串行化修改、必要耐久；读失败与损坏不得混同于"无未决进程"。
3. 以明确 finished/reaped 回执释放记录；`ChildLease::Drop` 只做保守清理，不伪造确认。
4. 监督配置由宿主统一注入普通 dispatcher 与 `RecipeProofRunner`（修 F03 的 `ProcessRunTool::new` 默认关闭）；re-exec marker 不隐式依赖传播到任意客户端可执行文件。
5. 明确组长/成员/正常解除语义：watchdog 在 exec 前加入孩子的独立进程组，持续保住组身份；Drop、终止 helper 与清理后的 wait 均有界，无法确认则保留责任。Windows session 沿用既有 Job 围栏，未确认退出不得生成耐久完成回执。

**已执行的定向检查（2026-09-07，当前工作树）：** Windows 进程库 38、工具库 246（另有 1 项原有 ignored；新增未知退出用例后 session 单独复验 14）、process_journal 8、依赖边界 3、compose 硬退出/监督门禁/恢复 6 项通过；上下文与 GC 23、Core 审批 1、Actor 工作入口 18 项定向复核通过。WSL Linux 的 watchdog 单元 6 / 真实子进程 4、监督台账 17、进程日志 7、session 14、process.run 19、exact-proof 宿主硬退出 1 项通过。相关 crate 的 all-targets 检查通过。完整命令与实现证据见 [核心边界与宿主清理报告](reviews/2026-09-07-worktree-review/CORE_BOUNDARIES_AND_HOST_CLEANUP.md)。

**剩余验收：** 正式 P3 宿主的同等接线与硬退出路径；spawn 到 Job/watchdog 就绪的窗口、OS 拒绝容器/监督启用、子进程主动脱离进程组，以及遗留无容器孤儿的冷恢复。当前 Windows 根句柄与 Unix watchdog 组身份保证各有明确范围，不能外推为所有后代遍历和冷恢复 PID 竞态均已消除；B1 尚未全部验收完成。

**做到这里停止：** 不新建通用 Scheduler；受支持的执行/恢复声明不得先于本工单验收。

## B2 — metadata 已发布但目录同步失败的围栏（已关闭 2026-09-07）

**用户结果：** 一次压缩返回不确定错误后，本进程不会继续健康地向旧代写入。

**已落地：** `persist_authority_metadata` 在 rename 发布之后才可能失败的目录同步改为 `AgentError::RecoveryRequired`（"可能已部分落地"语义）；`compact_locked` 对该变体设置 `writer.failed` 围栏——显式 `compact_authority_journal` 与追加触发压缩同走此路径，不能留下健康的旧代 writer。故障注入为 thread-local 的 `SYNC_DIRECTORY_FAULT` 切点（唯一发布后失败点），不触碰真实目录同步屏障。

**检查：** `cargo test -p agent-storage` 22/22（Windows，并行 ×3 稳定；WSL2 同过），含新增 `compaction_publish_uncertain_failure_fences_the_writer`：发布不确定错误 → 后续 append 被拒 → 重开承认已发布 g2 并继续追加（seq 3）。`cargo test -p agent-core` 全绿。续审确认"已有实质修复……本轮通过"（reviews/2026-09-07-worktree-review/REVIEW.md）。

**做到这里停止：** 只修发布语义；不删目录同步换测试绿；不新增数据库/Chronicle/第二套日志协议。

## B3 — Context 验证关联与运行边界

**用户结果：** 错误不被同实体的无关验证终结；外部输入/输出不在进入 Runtime 前后绕过限额。

**已落地（2026-09-07，F08＋F09 主体）：**
1. **验证关联（F08）：** `ContextItem`/`ExternalizedContext` 新增 `verify_recipe`（serde default，兼容旧检查点）；verify.run 失败把 `metadata.recipe_id` 盖到错误上，成功只有携带**同一 recipe_id** 才排队终结；`queue_error_verifications` 不再按实体重叠匹配，无关联的成功一律保持 live。
2. **读入计费（F09）：** `resolve_prompt` 的 stdin 路径 `take(USER_INPUT_REPLAY_MAX_BYTES+1)` 有界读入（读入阶段即拒，不做全量分配）；`load_grant_file` 改为 take 上限读取，stat 仅为 regular-file 检查。
3. **有界输出 sink（F09）：** `run_headless` 改为移交 writer 所有权，专用写线程＋64 行有界队列；事件循环不再被慢 stdout/文件阻塞（超时/取消保持有效）；慢消费者/断线以类型化失败结束（截断流绝不报 exit 0），关闭等待有界（10s）。

**检查（已执行，2026-09-07）：** `cargo test -p context-simple` 288 全绿（含新增 `unrelated_successes_never_finalize_an_error`：不同配方成功、普通工具成功均不终结，同配方成功终结）；lifecycle/residency fixture 更新为同配方契约。`cargo test -p agent-tui` cli 16 项全绿（含 `stdin_prompt_is_charged_at_read_time`、`grant_file_over_the_cap_is_refused`、`headless_output_disconnect_ends_the_run_with_a_typed_failure`）。

**剩余范围（续审指明，随 B3 后续/P2 收口）：** 可信 recipe 的版本/覆盖身份及任务、故障级关联保存与核对；无期限且未达 cap 的 stdin 读取期限；终端 IO helper 不当成正式 GUI 客户端实现。

**做到这里停止：** 不重调 GC 阈值，不同时引入 BM25/向量/缓存算法。

## P1 — 与 TUI 无关的原子工作提交与受理回执

**用户结果：** TUI、GUI、SDK 不能把指令误投给另一客户端刚切换的任务（修 F07）。

**入口：** `crates/agent-tui/src/work.rs`（现共享工作流）；`crates/agent-runtime/src/command.rs`、`crates/agent-runtime/src/actor/commands.rs`；`crates/agent-compose/src/lib.rs`。

**步骤：** 共享工作入口移入公共应用层；实现原子 start_work 或显式 task/expected revision 提交，复用既有 TaskManager prepare/commit；返回稳定受理身份，同 client request id＋相同内容有界去重、异内容拒绝；无可靠回执返回 unknown/要求查询，不自动换 ID 重放副作用。

**检查（建议，未执行）：** `cargo test -p agent-runtime`；`cargo test -p agent-tui`。验收：两客户端交错 SetFocus/Submit 不跨任务投递；重复请求不偷偷再执行；单客户端任务身份不变。

**做到这里停止：** 不远程导出整个 `RuntimeCommand`；不公开恢复半事务或 `CorePort`；不建第二个 TaskManager。

## P2 — 类型化快照、增量事件、审批与结果

**用户结果：** 新客户端接入、重连、慢消费后显示正确状态，不解析终端文字（修 F06）。

**入口：** `crates/agent-runtime/src/status.rs`（`anchor_revision` 跨任务 `max`）；`crates/agent-contracts/src/event.rs`；`crates/agent-core/src/approval.rs`；`crates/agent-tui/src/cli.rs`（文字推断审批）；`crates/agent-runtime/src/platform/`。

**步骤：** 修 revision 归属；移除从任意 ToolOutput 文本推断审批拒绝；一致快照＋watermark＋其后事件、有限重放窗口与 `resync_required`；实时文字流偏移与耐久事件序列分开；审批响应绑定 request/run/operation 与会话，重连可查 pending；慢消费者有界队列，不静默抹掉审批/终态、不阻塞 Actor。

**检查（建议，未执行）：** `cargo test -p agent-runtime`；`cargo test -p agent-tui`。验收：A revision9→B revision1 展示与 API 均为 B=1；工具正文含拒绝短语不改变真实审批状态；缺口被明确报告。

**做到这里停止：** 只建可重建投影；不建 Chronicle 数据库；投影不反向提交 effect。

## P3 — 正式 Rust 宿主与本地双向 RPC（已落地 2026-09-07）

**已落地：** `crates/agent-host`（成员已入 workspace）。薄宿主二进制镜像 TUI 组合根（同一 kernel/tools/approval/context 选择），`--pipe`/`--socket`/`--read-only`/`--restore-latest`，`host.lock` workdir 单实例（进程身份核对陈旧锁接管）。Windows 命名管道：`PIPE_REJECT_REMOTE_CLIENTS`＋仅当前用户 DACL＋逐连接客户端令牌 SID 校验；Linux UDS：SO_PEERCRED；无法验证的对端在首帧前丢弃（fail closed）。每连接服务端安装 WorkControlGrant（operator/read-only）＋独立绑定 authorizer，wire 字符串不自报授权。帧＝4-byte LE＋JSON、1 MiB 帽（与 .NET 客户端一致）＋协议 crate DOM 预算；畸形帧关连接；未知路由回 `route.unsupported`。事件通知 wire 契约随 P2 类型化事件落地，客户端先以快照重建（诚实不丢）。

**已验证：** `cargo test -p agent-host` E2E（命名管道）：提交受理/幂等重试 AlreadyAccepted/同 id 异 goal 结构化拒绝 `work.rejected`/快照焦点绑定/真实 gate.authorize 注入审批→快照可见→服务端绑定 id 应答 Delivered→挂起决策解析为 Allow/cancel 诚实 ack/未知路由拒绝。互操作冒烟：真实 .NET Agent.Client 连真实宿主默认管道——连接、快照、提交、焦点、取消 `NoActiveTurn`、干净退出（exit 0）。发现并修复 C# 转换器 HashSet 预置 "status" 导致合法字段被拒的 bug。

**未验证/限制：** UDS 路径仅代码＋编译，真 Linux 运行待 CI（Windows 开发机无 UDS）；事件 wire 契约未定义（订阅回 watermark，通知接收方为后续）；UDS 读期限有（120s），命名管道阻塞读依赖本地可信对端＋有界帧，未做读写期限。

**用户结果：** 原生 GUI 与其它入口连接同一工作区宿主，不各自打开一份可写运行状态。

**入口／拟新增路径：** `crates/agent-runtime/src/platform/session.rs`；`crates/agent-process/src/session.rs`；`crates/agent-platform-protocol/src/`；`crates/agent-compose/src/lib.rs`；`proposed: crates/agent-host/`。

**步骤：** 薄宿主可执行文件（生命周期、workdir 单实例、watchdog marker 在 Rust 宿主负责）；Windows Named Pipe／Linux UDS 同一有界 framing，OS 后端隔离；连接 ACL/peer 身份在服务端落实，客户端不得自报提权；帧与 decoded DOM 上限、并发/队列/读写期限；接收循环不等整个任务结束才读 cancel；关闭窗口、断线、宿主退出、task cancel 区分，提供显式后台继续或停止策略。

**检查（建议，未执行）：** `cargo test -p agent-platform-protocol`；`cargo test -p agent-process`；`cargo test -p agent-runtime`。验收：半帧/粘帧/超大帧/无权会话正确处理；第二客户端附着既有宿主；B1/B2 未完成时可开发只读连接，但不开放相关可靠执行/恢复承诺。

**做到这里停止：** 不做系统级常驻服务、不默认公网监听、不复制 codec 和 authority；不同时做 HTTP/TCP/gRPC。

## G1 — .NET 客户端库与正式 Avalonia 外壳（已落地 2026-09-07）

**已落地：** `clients/dotnet/Agent.Client`（net10.0 类库，零 Avalonia 依赖、零 P/Invoke）：run-scoped DTO 逐字节镜像 Rust `work.rs` wire 形状（snake_case、deny-unknown、serde 变体名原样 Only "Allow"/"Deny"），有界帧（4-byte LE、1 MiB），request-id 关联，本地等待取消与显式 `work.cancel` 命令分离，Named Pipe/UDS 传输，ResumableSession 重连策略。`clients/dotnet/Agent.Client.Tests`（24 项）与 `crates/agent-platform-protocol/tests/work_fixtures.rs`（9 项）读同一批 `tests/fixtures/work/*.json`：解码＋验证＋重编码逐字节一致（C0 双语一致性验收）。`apps/Agent.Desktop`（Avalonia 11.3.20）：任务列表/目标提交/继续/取消/审批卡/快照驱动状态条，异步 IO 不占 UI 线程，PerMonitorV2 manifest，无 WebView。`global.json` 锁 SDK 10.0.301。布局夹具模式明确标注"非执行器"。

**已验证：** `dotnet build` 两项目 0 警告 0 错误；`dotnet test` 24/24；`cargo test -p agent-platform-protocol` 35＋9 绿；GUI 6 秒真实启动冒烟；互操作冒烟（见 P3）。

**未验证/限制：** 事件流消费等待 P2 wire 契约（客户端订阅已实现，通知流为空）；中文输入法/DPI/大文本未做专项实测；plan/Context 面板等平台字段。

**用户结果：** 第一版就是正式原生客户端；通信层可被其它 .NET 应用复用。

**拟新增路径：** `clients/dotnet/Agent.Client/`、`apps/Agent.Desktop/`、`global.json`（均拟新增，锁定实施时确认的 .NET 10 SDK 版本）。

**步骤：** class library 与 Avalonia app；Agent.Client 不依赖 Avalonia、不 P/Invoke Runtime；DTO 用 C0 共同规范与跨语言样例；请求关联、取消等待与显式取消命令区分、事件流与帧上限；先用同 DTO 有限 fixture 驱动布局，P1/P2 可用后连真实宿主（fixture 不是模拟执行器）；正式窗口含任务选择、输入/输出、计划、状态，异步读写不占 UI 线程。

**检查（建议，未执行）：** `dotnet build clients/dotnet/Agent.Client/Agent.Client.csproj`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。验收：client library 可在无 GUI 测试程序使用；正式 GUI 连真实 Runtime 取快照并提交/继续；主界面不依赖 WebView。

**做到这里停止：** 不先做 IDE、插件 UI SDK 或全量自绘控件库；不用第二个"验证 GUI"替代。

## G2 — 审批、取消、审阅与恢复完整操作链（客户端＋宿主链路已落地 2026-09-07）

**已落地：** 桌面审批卡走平台绑定 id 并等真实回执（宿主 E2E 证明 Delivered/NoLongerPending 语义）；ResumableSession 断线后重连强制快照重建，`RespondApprovalAsync` 不自动重试（丢失的 Allow 可能已送达也可能没有，重发即伪造同意），重连横幅显式声明"挂起审批不会自动通过"；取消走 `work.cancel` 并如实区分 Cancelled/NoActiveTurn；冷恢复在宿主侧走完整 `RuntimeInstance::restore` 事务（`--restore-latest`），窗口重开即快照同步。

**已验证：** 宿主命名管道 E2E（审批全链路）＋ .NET 互操作冒烟；`dotnet test` 24/24 含"断线不自动应答审批"专项。

**未验证/限制：** GUI→真实工具→差异审阅端到端待差异/工件读取路由与真实 provider（不可用写 NOT_RUN）；预算让出/证据完成/操作员接受/恢复受阻的细分展示待 P2 类型化任务字段（快照目前只有 active/suspended/completed）；含用户预存修改的冷恢复 GUI 走查未跑。

**用户结果：** 用户能执行真实开发任务，并知道改动、检查、未验证状态与可恢复点。

**拟新增路径：** `apps/Agent.Desktop/`、`clients/dotnet/Agent.Client/`；`crates/agent-runtime/src/instance.rs`。

**步骤：** 审批卡显示平台返回的绑定操作/范围，点击等真实回执；区分运行/预算让出/待审阅/证据完成/操作员接受/恢复受阻；差异与工件按需授权读取，保留用户已有修改；重开走快照/事件同步，冷恢复走完整 `RuntimeInstance` 事务；中文输入法、复制、键盘、DPI、大文本正常。

**检查（建议，未执行）：** `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`；`cargo test -p agent-compose`。验收：GUI→任务→真实工具/审批→取消/续跑→差异审阅端到端；丢连接不自动通过 pending 审批；含用户预存修改的冷恢复不重复副作用。

**做到这里停止：** 不加第二执行器；不做全量通用代码编辑器；不因 GUI 增加绕过 Core 的文件写入口。

## G3 — 低资源使用的正式工作台与只读 Context 检查（客户端侧已落地 2026-09-07）

**已落地：** 列表虚拟化（Avalonia ListBox 默认虚拟化栈）＋底层保留上限（任务 200 条、输出尾部 400 行、通知队列 1024）；DeltaCoalescer 小窗口合并流式文字（不丢字符，事件契约落地后接线）；只读 Context 面板占位（平台路由前不显示推断内容）；MetricsSession 有界测量记录器（全进程树工作集采样，未测指标写 null=NOT_RUN）。首采样：[walkthroughs/2026-09-07-g3-desktop-metrics.md](walkthroughs/2026-09-07-g3-desktop-metrics.md)（空闲工作集 222–232 MB，单环境调试构建，不构成性能结论；长会话斜率/输出 CPU/大 diff 均 NOT_RUN）。

**用户结果：** 长会话、大 diff 和上下文查看不导致全历史反复传输、解析和渲染。

**拟新增路径：** `apps/Agent.Desktop/`；`crates/agent-runtime/src/platform/`。

**步骤：** 列表虚拟化同时限制底层保留数据；流式文字按小窗口合并；大正文只传 locator/元数据/分页片段，视图关闭释放缓存；Context 面板只读显示来源、表示类型、实际曝光、片段范围、恢复状态（不显示不存在的模型内部注意力）；记录全进程树空闲内存、长会话斜率、输出时 CPU/分配、大 diff 响应，无测量写 `NOT_RUN`；AOT/裁剪单独做兼容性核查，不作首窗前置。

**检查（建议，未执行）：** `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release`。验收：固定大型会话/差异 retained data 有界；工具/审批/终态事件不为流畅显示静默丢失。

**做到这里停止：** 不建遥测平台；不写零分配通用 UI；不以研究性 GC 算法胜出当 GUI 完成标准。

## E1 — 一个真实外部能力与按需 Skill 的最小闭环（已关闭 2026-09-07）

**用户结果：** 正式客户端能使用一个实际外围能力；Skill 不常驻所有上下文。

**已落地（2026-09-07）：**
1. **真实外部能力选型：MCP stdio server**（复用既有 `McpCapabilityAdapter`/`McpClient` 沙箱栈：scrubbed env、私有 cwd、landlock/integrity、supervisor 树杀）。
2. **MCP 取消贯通（MCP-01 范围，不以"默认未开启"豁免）：** `request_with_cancel` 的写阶段改为与 cancel select 组合（取消即时生效，部分帧按既有 poison+kill-then-reap 收尾）；新增 `initialize_with_cancel`/`list_tools_with_cancel`/`connect_stdio_with_cancel`——连接（spawn+握手）阶段可取消，取消不被重启熔断记为服务器故障。测试：写阶段取消（对端停读时 50ms 内 Cancelled，不等 30s deadline）、连接阶段取消（静默服务器 150ms 取消即收尾并树杀）。
3. **compose 配置缝：** `ComposeConfig` 新增 `mcp_servers`（配置→发现→注册→显式 enable；发现失败组合失败 fail-closed）与 `plugins`（注入 dispatcher）；风险从声明权限推导，从不信服务器自述。
4. **Skill 按需读取：** `PluginRegistry::install_from_root`/`skill_read`——双激活门（包 Active＋Skill Active）、引用路径围栏（准入拒绝逃逸 + 读取时二次防御）、64 KiB 有界读（超限拒绝不截断）；`capability.manage` 新增 `read_skill` op，正文作为普通工具输出带 provenance/version 返回，从不自动注入、从不成为 system 权威。
5. **闭环集成测试**（`tests/e1_mcp_loop.rs`，真实 mock server 进程）：配置→发现→注册→search（未加载工具不在模型表面）→load（进入表面）→invoke（有界回显＋心跳）→shutdown（心跳停跳＝进程树确死，清理可观测而非假设）。

**检查（已执行，2026-09-07）：** `cargo test -p agent-capability-process` 24 lib＋26 capability_host＋1 闭环（Windows 与 WSL2 双侧全绿）；`cargo test -p agent-runtime --lib plugin:: capability::tests::read_skill` 9 项全绿（含 read_skill 双门、逃逸拒绝、超限拒绝、无目录拒绝）。验收对照：新增能力零 RuntimeActor 特判（注册即被统一目录/加载/调用面消化）；未加载能力 schema 不注入（search 轮断言 mock.echo 不在表面）；停机清理可观测（心跳停跳断言）。

**做到这里停止：** 不建插件市场；不要求子 Agent/DAG/递归才完成本切片。

## R1 — Rust＋.NET 来源绑定发布及正式使用收口

**用户结果：** 安装包里的客户端/宿主确属本次构建，版本匹配，真实使用与验证范围明确。

**入口：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`；拟新增桌面打包；`docs/CURRENT.md`、`docs/COMPATIBILITY.md`。

**步骤：** 修自定义 target 未传 Cargo、陈旧 dist 混入、PowerShell 退出码（即 PACKAGE-01 范围，届时一并关闭）；记录 Rust 源码 SHA、.NET/协议版本、构建配置与 checksum；现有跨平台 CI 加最小 C# 构建/协议相容项；正式 GUI 完成小 bug、多文件功能、断线/冷恢复续跑（真实 provider 不可用写 `NOT_RUN`）；实现/定向测试/默认启用/真实使用四类状态分别更新。

**检查（建议，未执行）：** `cargo fmt --check`；`cargo clippy --workspace --all-targets -- -D warnings`；`cargo test --workspace`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release`。这些是集成/发布检查，不是每次改文档都执行。

**做到这里停止：** 不要求所有未来扩展实现才发布当前正式 GUI；失败构建不输出成功包。

---


---

## STORAGE-02：压缩发布后的 writer 围栏（已关闭 2026-09-06）

**实现：** `compact_locked` 的全部可失败步骤（新 WAL seek、next_seq 溢出检查、恢复态重建）移到 `persist_authority_metadata` 发布点之前；发布后只剩纯内存 writer 交换与尽力删除旧 WAL，结构上不可能再把 writer 留在旧代。新增定向测试：新 WAL 创建失败（发布前）保持旧代可追加且元数据未动；发布后/删除旧 WAL 前的崩溃窗口重开按已发布代际续写。agent-storage 21/21。

**用户结果：** 压缩元数据已经写到磁盘之后，若 seek、切 writer 或显式 compact 调用失败，本进程不能继续往旧代追加；再次打开与对账结果一致。

**入口：** `crates/agent-storage/src/lib.rs`（`compact_locked`、`persist_authority_metadata`）；`crates/agent-core/src/operation.rs`（`compact_authority_journal` 目前只转发错误）。普通 `append_transition` 已有围栏，不要拆掉。

**步骤：** 读 compact 成功发布 metadata 之后的失败返回点；失败后隔离旧 writer（或等价的本进程写入围栏）；打开路径与 STORAGE-01 一致：缺 metadata 且有代际则 `RecoveryRequired`，不选最大 `.gN`。

**检查：** `cargo test -p agent-storage`；若改了 Core 转发路径，加一条显式 compact 失败后不可再追加的定向测试。不跑全仓。不注入真实 Windows 进程崩溃。

**做到这里就停。** 不建 Chronicle；不把 STORAGE-01 的 Windows 崩溃注入当成本工单。

---

## PROCESS-01：宿主验证硬崩溃后的监督

**用户结果：** 开启宿主验证时，宿主进程被 SIGKILL/abort 后，不留下可继续改工作区的无监督子进程。

**已落地（2026-09-07）：** ① Windows KILL_ON_JOB_CLOSE 围栏（`7c72df3`）；② Unix 管道 EOF 看门狗（`a954314`：re-enter 宿主可执行文件 + socketpair，宿主死亡 → EOF → 确认组长仍活 → `kill(-pgid)`；正常 reap 后 drop 写端解除）；③ 监督台账（`995a457`：execute_invocation 经 ChildLease 记录每个子进程，正常路径释放、崩溃路径留行，compose 启动时对账清理后才复用工作区；pid 精确匹配、死条目清除防复用误杀）。**已验证（2026-09-07）：** 全部 cfg(unix) 测试在真实 Linux（WSL2 Ubuntu，真内核）执行通过——agent-process 40/40（含看门狗 6 项：EOF 杀活组、reaped leader 不杀、pid-0 拒绝、Drop 收割看门狗、管道接线）、agent-workspace 132 项（WORKSPACE-01 FIFO、WORKSPACE-02 句柄路径）、tool-runtime 229 项（台账对账杀 abandoned 子进程）。CI 重跑转为回归确认。

**入口：** `crates/tool-runtime/src/proof_runner.rs`、`crates/tool-runtime/src/tools/process.rs`。Linux 探针已证 OS 机制（父死子存），不是 Agent 集成测试。

**检查：** 定向 `tool-runtime` / 现有 crash fixture；监督身份与权限身份分开。不否定已落地的取消桥接（F3-6b）。

**不要：** 为这件事新建 Scheduler。未跑 Agent crash-resume 不写已闭环。

---

## PROCESS-02：未确认退出不清 pid（已关闭 2026-09-06）

**实现：** `reap` 返回类型化 `ProcessReapOutcome`，仅在确认退出后清 pid，未确认终态由 Drop 保留击杀责任；`kill_tree` 直接子进程 fallback 改为无锁 direct pid kill（原 `try_lock` 在 reap 持锁时必失败）。回归：持锁期间非 group-leader 子进程经 fallback 终止（`7c72df3`；agent-process 30）。

**用户结果：** `reap` 在 wait 失败或两次有界等待都超时时，不能报告清理成功并丢掉监督身份。

**入口：** `crates/agent-process/src/supervisor.rs`（`ProcessSupervisor::reap`、`kill_tree`）。

**检查：** 注入 wait 错误 / 第二次 wait 超时的定向测试。Unix group leader 不能当作本工单已证明。

**不要：** 用“再 kill 一次”冒充确认退出。

---

## WORKSPACE-01：普通 confined open 不阻塞 FIFO（已关闭 2026-09-06）

**实现：** 普通 open 带 `O_NONBLOCK`（普通文件/目录 I/O 不受影响），同句柄 stat 拒绝非普通文件/目录，staged 目标要求普通文件；`project_markers` 改为仅元数据探测（`fstatat AT_SYMLINK_NOFOLLOW`），根扫描不再打开任何条目。回归：无写端 FIFO 位于 `Cargo.toml` 不再卡住扫描（watchdog 测试；`17c5ded`，agent-workspace 98+5+3）。

**用户结果：** 项目标记探测遇到无写端 FIFO 时有界失败，不在同步 `open` 上卡死。

**入口：** `crates/agent-workspace/src/confined.rs`、`runtime_facts.rs`（`project_markers`）。recovery 路径已有 `O_NONBLOCK`，复用它。

**检查：** Unix FIFO 名为 `Cargo.toml` 的定向测试；普通文件、`.git` 目录、symlink/reparse 不被误伤。正文只进入支持的文件类型。

**不要：** 把所有标记都当普通文件打开。

---

## WORKSPACE-02：Windows 拒绝路径立刻接管 HANDLE（已关闭 2026-09-06）

**实现：** 六处 raw HANDLE 调用点（`open_root_handle`、`open_child_dir`、`open_existing` 两臂、`open_staged_for_cleanup`、`open_or_create_regular_file`）全部先 `from_raw_handle` 接管再 `check_not_reparse`，对齐 recovery helper 模式。既有 reparse 拒绝测试覆盖行为；句柄计数故障注入未做（`17c5ded`）。

**用户结果：** `check_not_reparse` 失败时句柄仍被拥有并关闭，拒绝仍然发生。

**入口：** `crates/agent-workspace/src/confined.rs` 多处 raw HANDLE。复用已有 recovery helper。

**检查：** 现有 Windows confined 拒绝测试仍通过。未做句柄计数故障注入则写明。

**不要：** 把“拒绝发生”说成路径逃逸已存在。

---

## PROVIDER-01：错误响应有界读取

**用户结果：** 非 2xx 的巨大或持续 body 在读入阶段被限额，不先 `text().await` 再截短。

**入口：** `crates/provider-openai/src/lib.rs`（`complete_chat_stream` / `complete_responses_stream`）。

**检查：** loopback 夹具：大 body、持续 body、多字节 UTF-8；取消与超时仍正确。

**不要：** 改模型协议权威；无压力测试不写已抗资源耗尽。

---

## PROVIDER-02：Chat length 终止

**用户结果：** Chat `finish_reason=length` 与 Responses 输出上限一样，保留不完整终止，不丢成正常完成。已暴露的输出不透明重放。

**入口：** `crates/provider-openai/src/sse.rs`、`lib.rs`、`responses.rs`。

**检查：** Chat length+DONE 与 Responses `max_output_tokens` incomplete 成对夹具。

**不要：** 无条件重试。

---

## PROVIDER-03：Responses EOF 尾帧校验

**用户结果：** 最后一帧无论是否空行结束，event 名与 JSON type 矛盾时都拒绝。

**入口：** `complete_responses_stream` 的 `framer.finish()` 路径；与正常 SSE 共用 handler。

**检查：** 同一矛盾帧有/无尾部空行的夹具。

---

## CONTEXT-01：依赖候选 newest-first

**用户结果：** 同一实体超过 64 条 live 条目时，候选仍按 newest-first，而不是先截创建序旧前缀再排序。

**入口：** `crates/context-simple/src/index/dependency.rs`（`push_linked`）、`indexes.rs`（`update_entities` / `swap_remove`）。

**检查：** 64/65/128 条；dead 前缀不耗尽配额。扫描工作量与候选配额分开。

**不要：** 重写选择器或调 GC 阈值。关联边不是强制正文 Continuation。

---

## PACKAGE-01：打包来源绑定

**用户结果：** 一次发布复制的二进制就是这次构建写出的那份；旧 `dist/<version>` 和自定义 target 不能拼出“成功”的旧包。

**入口：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`。Bash 桩测已证明旧产物可被打包。

**何时做：** 下次实际发布或主动打包装时，不要提前改脚本充数。PowerShell 原生退出码一并核对。

**不要：** 宣称当前已发布 ZIP 已错；不为它新建评测框架。

---

## MCP-01：MCP 取消覆盖写阶段

**用户结果：** 对端停读时取消能打断写请求，不等完整 `request_timeout`；半帧 session 被毒化；清理状态有 reap 依据。

**入口：** `crates/agent-capability-process/src/mcp.rs`（`request_with_cancel`）。

**何时做：** 仅当默认产品声明启用 MCP 写路径。当前未启用则跳过，不算阻塞队列里的代码项。

**不要：** 第二调度器。

---

# M16：可持续交付的本地单 Agent

审查剩余关闭后再推进这里的产品剩余。阶段目标不变：用户给仓库级任务，Agent 能短计划、查读修改、补充、有限执行、停止、冷恢复、可审阅结果，以及同一 Runtime 的非交互入口。

提案原文不进默认必读：[reviews/2026-09-06-m16-proposal/TRIAGE.md](reviews/2026-09-06-m16-proposal/TRIAGE.md)。

| 工单 | 交付物 | 本分支状态 | 旧映射 |
|---|---|---|---|
| M16-00 | 切换活动路线 | 已切换 | D0 |
| M16-01 | 继续、忙时补充、启动预检 | 已关闭（2026-09-07）：走查转为自动化 TUI E2E | F1 |
| M16-02 | `/work` `/plan` 与完成语义 | 已落地（2026-09-07：待审阅/持久完成显式区分） | F2 |
| M16-03 | 有限模型轮与可确认取消 | 主体已落地；PROCESS/PROVIDER 见上列 | F3 |
| M16-04 | 可信冷恢复 | 已关闭（2026-09-07）：产品配置冷恢复走查落地（`e6795ed`） | 恢复路径 |
| M16-05 | `/review` 与状态区分 | 已关闭（2026-09-07）：双工作区走查转为无头 + TUI E2E | F4 |
| M16-06 | 上下文/搜索正确性 | 主体已落地；CONTEXT/WORKSPACE 见上列 | F5 |
| M16-07 | 单进程非交互入口 | N1/N2 已落地 | 原 F6 之后项 |
| M16-08 | 试用包与三类走查收口 | 无头 live 已有；TUI 交互走查已自动化（E2E）；PACKAGE-01 见上列 | F6 + 发布 |

---

## M16-00：切换活动路线

**用户结果：** 开发者进入仓库后看到一条当前队列，不会回到 M15 候选选择或 Chronicle 建设令。

**已做（2026-09-06）：** CURRENT / ROADMAP / 本文件改为 M16；随后把深入续审仍开放项提到本队列前面。提案原文入库审查目录。文档检查：`python scripts/doc_consistency.py`。

---

## M16-01：接通交互控制与可信启动

**用户结果：** 能继续原任务；忙时补充能看到受理/排队/拒绝；纯配置错误不先建 workspace 状态。

**已落地：** `/continue` → `continue_active_task()`；忙时单槽队列；命令错误走 notice；未知 flag 拒绝；release 消费盖章与 PromptRequired 判重（`c6fbbab`）。参数解析在打开 workspace 之前。

**已全部落地（2026-09-07，含预检收口 `fa2b6d1`）。** 原手工走查转为会话循环自动化 E2E（`tui_e2e_*`）：恢复后 /continue、忙时第二条输入可见排队并在下一 turn 应用。模型配置校验已前移到 workspace 创建之前（doctor 保持无需 key、先于其退出），参数解析、冲突检查、grant 校验、模型校验现在全部是纯预检；守卫是真实二进制测试 `real_binary_startup.rs`（坏 key 报错且不留 `.focus-agent`；AGENT_DEMO 真实二进制 headless 跑通，session_end 带待审阅语义）。不抽 CompositionPlan。

**不要：** 用 `user_message("继续")` 代替 continue；无界输入队列；后台服务。

---

## M16-02：任务工作模式、短计划与完成语义（已关闭 2026-09-07）

**已全部落地。** 入口（`/work`、`/plan`）与完成语义展示均已关闭；细节见下。

**用户结果：** 一个开发目标有短清单；用户能区分「已产出待审阅」「本段结束」「证据完成」「操作员接受」。

**已落地：** `/work`（focus + 空需求集上 `task.manage` PreferSurface + 一次 user-message）；`/plan` 读只读 `TaskPlanView`；清单随检查点往返。

**本切片剩余：已全部落地（2026-09-07）。**

1. 默认 `OperatorClosureOnly` 在 TUI / 无头结果里明确显示为待操作员审阅关闭。普通 final 结束 turn，不显示为持久 `TaskCompleted`。**已落地：StatusProjection 任务行带 [awaiting operator review] / [durably completed (operator accepted)]；TUI turn 结束时对活动任务显式提示；无头 session_end 增 `task_state` 字段（operator_accepted / awaiting_operator_review / none）。**
2. `/done` 走既有操作员接受路径，不伪造验证 PASS。**既有路径未动。**
3. `EvidenceRequired` 仅在宿主已声明准则与 coverage 时使用；普通 `cargo test` / `npm test` 保持 `TaskScoped`。**既有语义未动。**
4. `next_action` 仍是建议；不增加“必须为空才能完成”。计划 `[x]` 不是证据。**未变。**

没有可信验收域时，把结果交给用户审阅就是完整产品行为，不要为此新建通用验证器。

**检查：** 定向 `agent-tui` 状态投影 / 无头 `session_end`；复用现有 OperatorClosureOnly / task.manage 回归。不跑全仓。

---

## M16-03：有限执行段与可确认的进程取消

**用户结果：** 较长任务在有限模型轮后安全让出并显式续跑；慢命令/验证不会让控制入口失去响应。

**已落地：** `--max-rounds` 按模型轮解析；RoundBudget 让出并落安全点；EOF 后仍守超时/取消；验证取消 token 桥接 Actor。

**代码缺口改走前列 PROCESS/PROVIDER。** `--defer-proof` 改产品默认仍等真实慢验证走查。

**不要：** UI 自动无限续跑绕过用户上限；独立 Scheduler / worker pool。

---

## M16-04：可信冷恢复与最小历史入口（已关闭 2026-09-07）

**用户结果：** 关掉进程后能选经过验证的检查点，恢复同一任务并继续；缺失 metadata 不能当成新工程。

**已全部落地：** STORAGE-01/02；`RuntimeInstance::restore` 完整 prepare/finalize 事务；`--restore=latest`；产品配置「保存 → 结束组合 → 新组合 restore → continue」走查落地为 `agent-compose/tests/m16_restore.rs`（只读基策略 + standing grants 非 permissive 审批、capability-aware、持久预留 journal 跨重启、启动对账台账；两段各写一次、互不冒认）。`crash_resume.rs` 保留真实子进程崩溃矩阵。

**不要：** Chronicle / RunCatalog 数据库。

---

## M16-05：结果审阅与统一状态展示

**用户结果：** 能看到改了什么、哪些检查真正执行、哪些未验证；`/review` 不调模型、不绕过 Core 跑工具。

**已落地：** 事件派生结果卡；不归属用户原有修改；resync 最新优先/当前 run/水位/有界读；run_summary 按 entries+omitted。

**已全部落地（2026-09-07）。** 双工作区走查转为自动化 E2E：无头变体（`e2e_bug_fix_preserves_user_modifications`）+ TUI 交互变体（`tui_e2e_review_attributes_the_agents_change_not_the_users`，结果卡只归属本会话工具写入、不认领用户文件）。展示继续分清待审阅 / 预算让出 / 持久完成 / 恢复受阻，不能只凭 `TurnCompleted` 或 `RunCompleted` 推断业务完成。

**不要：** 全仓 `git diff HEAD` 据为 Agent 成果；diff 编辑器；自动 rollback。

---

## M16-06：上下文、GC 与搜索的实际编码闭环

**用户结果：** 跨文件切换能找回正确片段；搜索不完整不冒充全仓无命中。

**已落地：** fs.read 区间事实；同修订窗口覆盖才取代；grep/list PARTIAL；confined 有界读；错误仅 verify.run 可清。

**已全部落地（2026-09-07 复核收口）。** CONTEXT-01/WORKSPACE-01/02 已关闭；「最终曝光 ACK 与片段身份一致性」经复核已由落地提交覆盖——`7a8a663`（消费台账改由最终渲染帧重建：最终请求中被裁掉的正文不再计为已选中）、`c5f2ab7`（保守片段身份）、消费 stamping 在 release 构建同样执行（engine.rs stamping 无条件、debug_assert 仅验返回值）。不重写 Frame 编译器。

**不要：** 向量库、BM25 新服务、SIEVE/TinyLFU、learned router 作为本阶段必需。

---

## M16-07：可脚本调用的单进程运行入口

**用户结果：** 不用 TUI 也能跑有边界的任务，得到 JSONL 与诚实退出码。

**已落地：** 同一 `agent-tui`：`--prompt` / `--work` / `--continue` / `--grant-file` / `--jsonl-out`。无人审批时拒绝写入；无 `--yes`。退出码 0/2/3/1。

**仍待做：** 若 M16-02 区分了待审阅，无头 `session_end` 与之对齐；不要为脚本退出 0 把普通 final 写成 verified completion。不新增 daemon。

---

## M16-08：发布可日常试用的版本并结束本阶段

**用户结果：** 对应本次源码的 Linux/Windows 包；三类真实任务记录可审阅。

**已有：** 无头 live 三类工作区记录；空 recipe 表启动失败已修。

**PACKAGE-01 走前列（下次实际发布）。** TUI 交互走查已由 `tui_e2e_*` 覆盖（含一条带用户原有修改）。真实二进制 live 记录：**NOT_RUN**（环境无凭据；`real_binary_startup.rs` 已用 demo transport 走通真实二进制路径，记录见 `walkthroughs/2026-09-07-live-binary.md`）。诊断导出补齐 M16-05 验收：常见凭据形状防御遮蔽 + 分享前审阅声明。

**不要：** 为凑 PASS 放宽标准或扩建评测框架。M16 结束不等于 v0.2 已发布，除非另有明确发布动作。

---

## 防止再卡在测试/文档循环

- 每个工单先给出一个用户动作；测试或文档单独增加不算功能完成。
- 人工走查默认转为不依赖人的端到端测试：交互路径走会话循环 `tui_e2e_*`（脚本按键 + 帧捕获 + 真实 compose），无头路径走 `run_headless` E2E；真实 provider 的 live 记录是另一回事，不可用则 `NOT_RUN`。
- 开发跑相关回归；集成沿用现有 CI。
- 回执写清：实现了什么、是否默认产品路径、实际检查、是否真实任务、限制、下一工单。
- 安全与持久性不能靠“快点落地”绕过；真问题修当前路径。
