# B 份：执行核心、恢复与持久结果

目标：LLM 的每次执行段都能达到合法安全点；超过热窗口仍可继续/恢复/审阅；低权限正文不能改变恢复读授权；慢 GC/保存不会夺走控制入口。

**本次先交付 EXEC-5，一个切片后停止。** 随后 EXEC-6 → EXEC-7 → EXEC-8，并收口 EXEC-4 的启动读取残余。依据见 [REPORT.md](REPORT.md) 的 R2-01/02/08/09/12。

先读 CURRENT/NEXT_TASKS 顶部，核对 HEAD 与工作树摘要。当前前序修复尚未提交，不从裸 HEAD 开工遗漏它们。B 继续单一合入 contracts/protocol/command/compose/公共 DTO/Actor 发射点；A/C 提交具体需求和 fixture，领域结论由提出方确认。

## EXEC-5 / P1：热投影、完成权威与检查点保持一致

**用户结果：** 完成第 65 个及之后任务时仍能保存、完成下一任务并正式冷恢复。

- 入口：`TaskManager::enforce_hot_bounds`、`prospective_terminal_snapshot`、`TaskManagerSnapshot`、`RuntimeCheckpoint::validate`、safepoint/terminal commit。
- 先在真实组合中做 64/65/66 次合法完成，随后保存/解码/restore/continue；同时保留单元反例：64 task rows＋65 completion records 违反既有校验。不要只测 serialized bytes。
- 定义一致的有界热视图与保留的最小完成权威/持久定位。裁剪不能产生孤立回执、丢 anchor revision 或删未决任务。`snapshot.validate()` 的关系约束要保留；不能通过接受孤立记录让测试变绿。
- 必要回归：64/65/66/256/257/1000 边界；夹杂 Active/Suspended；完成失败后的原状态；prospective 与实际提交后状态一致；旧完整 checkpoint 恢复再保存。完整链路使用现有 host/compose 路径，不只调用 TaskManager 私有测试 helper。
- 定向验证：runtime task/checkpoint/instance completion 与相关 compose 冷恢复。前序记录的 safepoint 失败单独复核归因，不能自动当本项证据或无关噪声。
- 停止条件：有界且关系合法，下一任务能够持续完成/保存/恢复。旧结果用户可查由 EXEC-8 接续，不提前宣称全部审阅功能完成。

## EXEC-6 / P1：类型化恢复引用、Unicode 安全与读授权

**用户结果：** 有效恢复保留必需旧产物；正文中的伪引用不会增加读权限，坏引用不会使恢复崩溃。

- 入口：`actor/restore.rs::collect_checkpoint_recovery_roots/extract_protected_run_ids`、artifact lineage、typed checkpoint/artifact locator、恢复状态投影。
- 先复现合法 checkpoint 正文中 `artifact://run/`＋35 个 `a`＋`汉` 的切片错误，再构造用户/工具正文嵌入其他 run URI 的反例。
- 先 decode/validate，再从明确的 typed captured references 取证；验证真实 sealed locator、来源 task/run、继承谱系。**完整性 checksum 不等于任意正文获得权限**。存储 GC 的“保留哪些文件”与当前 run 的“可以读哪些祖先”是不同集合。
- 必要回归：无关保留 checkpoint、quoted/tool-text 伪 URI、无 digest、错 task/run、Unicode/坏 UUID/超界、33/64 次合法恢复、登记失败/部分接纳。合法旧 sealed artifact 仍可分页，完成证明保持更严格的独立校验。
- RestoreEvidenceDegraded 要成为 LLM/快照/SDK 可以重新取得的有界事实；当前 TUI 日志能看到不等于后接入客户端或模型也知道。根集不完整时如实降级，不把部分列表当完整授权。
- 停止条件：权限来自已验证来源，解析总是类型化成功/拒绝而非 panic；不增加副作用重放权。

## EXEC-7 / P1：GC 与检查点维护等待不占控制入口

**用户结果：** 模型已结束但 GC/压缩/保存慢时，取消/停止仍有可解释且有界的结果。

- 入口：`turn.rs::finalize_after_model/execute_directive`、`lifecycle.rs::compact_after_completion/run_storage_gc_at_boundary`、`safepoint.rs::assemble_checkpoint`。
- 先用真实 SimpleContextEngine＋gated store 停在 full GC；另用真实 Rolling＋gated compactor 停在 checkpoint maintenance，发送 cancel/stop/status。Materialize 新测试通过不代表这些路径通过。
- 复用现有 operation/continuation，明确每个等待的 plan/IO/commit 所有权、generation fencing、abort/join、失败围栏；A 的分批 GC 接口在这里接线。同步大扫描也需要单次工作界，不能仅 tokio::spawn 后宣称响应有界。
- 必要回归：GC/保存各 await 边界的取消竞态、已提交 effect 不重复执行、物理删除不伪称回滚、late completion 不修改新 turn、正确保留完整 directive、checkpoint debt 未被错误清除。未确认清理返回 RecoveryRequired，不能提前发可信停止。
- 停止条件：已定位的长等待不占 Actor 命令分支，所有最终/恢复/持久屏障语义不变。不开第二调度器或并行 worker pool。

## EXEC-8 / P2：完成任务退出热表后仍可按身份审阅

**用户结果：** 旧任务不在热窗口时，用户仍能查看其结果、原产物和证据，或明确知道保留期/缺失状态。

- 入口：TaskDetail/结果读模型、已存在 journal/artifact 的有界回读、平台/SDK 与现有 GUI 审阅入口。先依赖 EXEC-5 的一致定位关系。
- hot miss 后走只读持久定位，返回 paged/cursor 与完整性；区分 unknown task、已退休但可查、已过保留期、损坏。不得全量加载整段 trace 或从全部 git diff 认领结果。
- 必要回归：超过 64/256 窗口后分别查询旧 task ID；冷重启后同结果；有用户既存修改；缺失/损坏 artifact；请求预算耗尽可继续分页；验证零模型/零工具副作用。
- 停止条件：旧结果真实可取得并绑定原 task/run/digest。磁盘上“也许还有日志”不能当功能验收。

## EXEC-4 残余 / P2：启动入口使用同一个有界读取契约

`decode_checkpoint_file` 仍先 `std::fs::read`。修共享 helper，TUI 启动显式 `--restore` 和 latest 都覆盖；保留已经修好的交互 `/restore`。测试正常值、cap/cap+1、元数据检查后增长、UTF-8/坏 JSON/坏 checksum，并从真实启动入口确认超界在分配阶段拒绝。文档按“交互/启动/平台”分别写清，不把一个 helper 通过外推到全部入口。

## 交付约束

执行者一片一交付，定向检查后沿用既有 CI。共享接口按实际需求小幅变更，fixture 双侧对齐；CorePort、恢复半事务和全 RuntimeCommand 不导出。业务完成仍由现有模式与操作员/证据权威决定，不能用多跑测试取得自动完成权。

本轮任务书不授权调用付费模型或发布。COST-5 最后核对真实默认入口、长时故障恢复、费用完整性与冷审阅；当前未跑的检查/真实环境必须写 NOT_RUN。
