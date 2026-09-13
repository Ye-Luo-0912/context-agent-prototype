# M18 第二轮：GC、上下文与配套核心全仓续审

日期：2026-09-12。HEAD 仍为 `685b6bbb29275bc8ec73ce6625a94567a8b8d23d`，**审查对象是该 HEAD 加当前未提交工作树**。相对上一轮源码快照已有 70 个文件变化、1 个新增文件；旧结论不能只靠同一个 SHA 沿用。[变更清单](CHANGES_SINCE_PRIOR_AUDIT.json)。

本轮只读产品源码并编排任务，没有修改实现、运行 Rust/.NET 测试、调用付费模型或提交发布。实施回执中的“通过”属于前序会话；本轮的问题证据为源码调用链与具体反例，均尚未执行运行复现。

## 结论与三份任务

继续 M18，目标仍是长期稳定、执行高效可靠、同质量全成本下降。沿用三部分：**A 上下文/GC/检索，B 执行核心/恢复/持久结果，C 成本/缓存/维护预算**。本轮定位 **12 项问题：5 P1、7 P2**，分为新改动的跨模块回归、前序切片残余和此前未深入的 GC 路径。不是把上一轮 8 项原样重开。

三份可直接交接的执行任务：

- [A：上下文与 GC](TASK_A_CONTEXT_GC.md)：首片 CTX-5，随后 CTX-6/7/8/9。
- [B：执行核心与恢复](TASK_B_EXECUTION_RECOVERY.md)：首片 EXEC-5，随后 EXEC-6/7/8，并收口 EXEC-4 的启动入口残余。
- [C：缓存与成本](TASK_C_COST_CACHE.md)：首片 COST-6，随后 COST-7/8；原 COST-5 负责最后的共同验收。

共享契约仍由 B 单一合入，A/C 提交最小字段与失败语义需求。一次只交付一个切片。RuntimeActor、Core effect 权威、ContextEngine 替换性、语义终态和恢复引用边界保持。

## 覆盖方式

本轮对 20 个 Rust workspace crate、.NET SDK/桌面、测试与构建入口的 **418 个文件、285,276 行**完成源码摘要盘点和词法结构扫描。详细追踪 GC 的 mark/sweep/recall/externalize/retry/storage-delete、四种正文位置、材料化/消费/作用域、检查点验证/恢复谱系、成本全链及其已有测试。辅助模块与前序修复按当前实现定向复查。

**不是 285,276 行逐行语义审核，也不是全仓无缺陷证明。** 词法扫描不是测试；候选搜索命中不是缺陷。[覆盖分层](COVERAGE.md)、[源码摘要](SOURCE_MANIFEST.json)、[结构扫描](STRUCTURAL_SCAN.json)、[阅读请求范围](READ_RANGES.jsonl)、[源码漂移检查](SOURCE_DRIFT.json)可供下一执行者复核。阅读日志不包含全部早期 shell 读取，部分宽输出曾被工具截断；各结论关键分支均窄范围复读。

## 前序实现复核

| 前序项 | 当前看到的实现 | 本轮结论 |
|---|---|---|
| CTX-1 / E01 | 未知窗口不再从 identity-only 获得覆盖权 | 原反例修复路径保留，不重复派发 |
| CTX-2 / E02 | instead/revert 捷径已收窄，增加否定/保留判断 | 原反例不原样重开；语言覆盖仍需真实质量验收 |
| CTX-3 / E04 | 输入按来源选择，未选来源不建依赖，旧卡仅在 source_ids 中时替代 | 已有进展；单条超长来源与失败累计卡列为质量验收边界 |
| EXEC-1 / E03 | Materialize 已进入现有 operation lane，并有 abort/join | 材料化原等待点已移走；GC/checkpoint 仍有独立等待点，见 R2-08 |
| EXEC-2 / E06 | 热任务/回执分别裁剪到 64/256 | 新的检查点结构冲突 R2-01；冷结果访问未接通 R2-09 |
| EXEC-3 / E07 | 增加保护 run 集与 RestoreEvidenceDegraded | 保护来源使用原始字符串扫描，带来 R2-02；不等于可信引用闭环 |
| EXEC-4 / E08 | TUI 交互 restore、lineage、run-summary 已加入有界读 | 共享启动 helper 仍整文件读取，见 R2-12 |
| COST-1/2/3/4 | 压缩身份事件、普通失败 unknown、Chat hit 映射、稳定前缀、请求输出 cap、独立维护 timeout 已有代码 | 计费字段语义、全调用完整性及可配置累计预算仍未收口，见 R2-10/11 和 C 任务 |

## R2-01 / P1：热任务裁剪破坏检查点的任务—完成记录对应关系

**对执行者的影响：** 长期完成任务后，下一次安全点/完成提交无法生成合法 checkpoint，后续恢复和继续被阻塞。

**证据：** [task.rs](../../../crates/agent-runtime/src/task.rs) 2067–2094 保留 64 个 Completed TaskRecord，却保留 256 个 CompletionRecord；[checkpoint.rs](../../../crates/agent-runtime/src/checkpoint.rs) 740–777 要求每份 CompletionRecord 必须对应存在的 Completed task。第 65 个完成任务提交后，热表已移除一条 task，回执仍在；后续普通快照必然含孤立回执。[safepoint.rs](../../../crates/agent-runtime/src/actor/safepoint.rs) 339–351、540–545 在写入前执行该校验，因此不是只影响离线检查器。

**缺失的验证：** `the_checkpoint_task_snapshot_stays_bounded_across_a_thousand_completions`（task.rs 2751–2771）只检查数量和 `serde_json` 大小，没有构造完整 RuntimeCheckpoint 运行 validate，也没有真实完成—保存—冷恢复链。

**归属：EXEC-5。** 有界投影必须保留完整的权威关系与历史定位，不能放宽“每个回执对应一个合法完成任务”的校验来掩盖。验收覆盖 64/65/66/256/257 边界、下一任务的完成与恢复。

## R2-02 / P1：恢复保护 run 从原始正文扫描产生，且可在 UTF-8 字节切片处 panic

**对执行者的影响：** 一条普通内容中的伪 artifact 字符串能影响恢复后的可读 run 集；包含非 ASCII 的坏引用还能使恢复收尾或完成边界崩溃。

**证据：** [actor/restore.rs](../../../crates/agent-runtime/src/actor/restore.rs) 340–355 在完整 RuntimeCheckpoint decode/validate 之前扫描 payload；416–435 对所有文本查找 `artifact://run/`，直接截取后续 36 **字节**，只解析 UUID。没有区分 typed artifact 字段、用户正文、工具输出或无关保留 checkpoint 的字符串。结果通过 249–250 进入 [workspace/lib.rs](../../../crates/agent-workspace/src/lib.rs) 的 protected lineage 接纳，进而参与 `open_artifact_for_run` 的跨 run 读判定。

**两个独立反例：**

1. 有效 checkpoint 的 user/tool content 中放入 `artifact://run/<另一个有效 UUID>/...`，它与真正捕获的 sealed ref 一样进入保护集；集合还汇总所有保留 checkpoint，而非只证明当前恢复来源的祖先关系。这里不能把“正文里提到”提升为读授权。现有 sealed digest/confinement 仍会检查，未因此证明任意路径可读。
2. 文本包含 `artifact://run/`、35 个 ASCII `a`、再接 `汉`。`start+36` 落在汉字内部，`text[start..end]` 会先 panic，根本到不了 UUID 解析失败分支。

**归属：EXEC-6。** typed decode 后从经验证的来源/引用字段提取，并证明恢复谱系；储存保护根与读权限集分开。解析必须有界、Unicode 安全。失败/容量不足进入可查询的类型化降级，不能仅丢一条客户端可能没订阅到的事件。

## R2-03 / P1：必需证据根没有贯穿老化与语义终结

**对执行者的影响：** TaskAnchor 仍要求的正文可能被当成普通过期信息逐出或终结，模型随后只能反复补读或报告 required miss。

**证据：** [context.rs](../../../crates/agent-contracts/src/context.rs) 1477–1503 声明 PromptRequired/ResidentRequired 的在线持有义务。[full/mod.rs](../../../crates/context-simple/src/gc/full/mod.rs) 604–619 将它们 mark；但 191–195 的 `!aged_ordinary` 在根检查前否决存活，745–755 没有考虑 anchor。Warm 召回 892–899 却明确对 anchor 豁免，前后不一致。

此外 [minor.rs](../../../crates/context-simple/src/gc/minor.rs) 303–339 的 Warm TTL 与 [residency.rs](../../../crates/context-simple/src/residency.rs) 123–135 的 Resident TTL 只检查 pin/keep_alive/lease，不接受当前 anchor 保护。根声明仅写入 state（engine.rs 2150–2159），没有同步到这些终结判据。

**反例：** live 的 Working 普通正文被 ResidentRequired 持有，无 hot/lease，超过 `ttl*4` 后 full GC 仍将其逐出；live 的非最新 Ephemeral 正文被 PromptRequired 持有，跨 TTL 的 minor 可先将其 Tombstoned。已有 root 测试主要调用 gc，没有覆盖 claim→跨轮 maintain→gc→materialize 的整条链。

**归属：CTX-5。** 防止仍 Live 且有有效义务的记录被启发式误终结；已经 Superseded/VerifiedFixed/Tombstoned 的记录仍不得复活。强度区分保持，StorageRequired 不自动变成常驻正文。

## R2-04 / P1：Pending 外置重试拥有正文，却漏掉多条语义与材料化路径

**对执行者的影响：** 存储短暂失败后，旧要求/已修错误仍能被搜索；同一条实际存在的必需正文又会被报告 Missing。

**证据：** `State.pending_externalize_retry` 是正文 owner；[store.rs](../../../crates/context-simple/src/store.rs) 504–537 的 fetch/search 明确读它，catalog 也登记 Pending。[reachability.rs](../../../crates/context-simple/src/gc/reachability.rs) 的决策替换 478–543、验证终结 554–599、终态提交 838–909 只覆盖 Resident/Warm/Stored，没有 Pending。[materializer.rs](../../../crates/context-simple/src/materializer.rs) 1190–1305 的 Pinned/PromptRequired 查找亦不查 Pending；[directive.rs](../../../crates/context-simple/src/directive.rs) 的 admit/derive 和 engine 的 lease/hint 还有同型遗漏。

**反例：** 把 store 路径设为不可写，使旧决策进入 Pending；随后发送明确替换。旧决策不进入 supersession，搜索仍按 Live 返回，写盘恢复后继续携带旧语义。若给该 Pending item 加 PromptRequired，正文就在内存里，required planner 却找不到它。

**归属：CTX-6。** 对唯一 owner 做统一解析，再让终态、required、admit、scope promotion 和 quota 使用同一位置语义；不另建一份状态权威。故障恢复期间不能给旧正文跳过新版本/终态检查。

## R2-05 / P2：Stored 元数据已提升，重新读入却恢复了 blob 中的旧元数据

**对执行者的影响：** inspect 显示的归属/持久性与真正 fetch/admit/GC recall 后的条目不一致，旧作用域和保留策略可能重新进入工作集。

**证据：** [scope.rs](../../../crates/context-simple/src/scope.rs) 419–478 在外部 entry 上更新 scope、scope_id、retention、Promoted tag。blob 内容摘要不变，磁盘 ContextItem 仍保留外置时的元数据。[engine.rs](../../../crates/context-simple/src/engine.rs) 1949–1979 fetch 直接返回解码 item；[directive.rs](../../../crates/context-simple/src/directive.rs) 130–159 admit 直接使用解码 item；[full/mod.rs](../../../crates/context-simple/src/gc/full/mod.rs) 473–500 recall 也没有合并当前 entry 的提升字段。

**反例：** 一条已 Stored 的可提升记录在 Task/Focus 关闭时被提升到父作用域，再 fetch/admit/recall；实际正文 item 与目录的归属不一致。后续 GC 根据旧关闭 scope 或旧 retention 决策。这里没有把通过 checksum 的原始内容称为损坏，问题是当前元数据被旧快照覆盖。

**归属：CTX-7。** 明确 immutable blob 内容与当前 owner 元数据的合并规则；读后仍按同一 owner/digest 复核。不能用旧 blob 的 Live 状态覆盖目录终态，也不能改写创建时钟伪装新鲜。

## R2-06 / P2：GC 写失败仅转移积压位置，整体资源和一次 full pass 仍无界

**对执行者的影响：** 磁盘故障持续时，模型还能继续制造历史；Warm 数量看似有界，Pending 和临时序列化副本却继续增长，GC 越来越慢。

**证据：** [full/mod.rs](../../../crates/context-simple/src/gc/full/mod.rs) 291–302 将全部溢出追加到 Pending，再序列化全部 Pending；328–369 将计划变为 map 并逐项 clone。Semaphore 限制同时 I/O 数，不限制本次总工作、排队结果与总字节。失败 359–362 被吞入重试，报告没有对应故障/背压状态。`gc_work_batch` 主要限制 minor/aging，并不覆盖整个 full plan/commit。

同一 full sweep 的 `marked.contains` 在逐条循环里查 Vec（194），根多时存在二次工作；已有 10,000 roots 测试只验证 mark_roots 数量，没有覆盖 sweep 的该成本。报告最终 truncate 也不能证明收集阶段没有峰值。

**归属：CTX-8。** 限 Pending 总条目/字节、一次 pass 的工作/序列化/I/O 配额，保留剩余所有权并公平续做；达到无法安全保留的容量前向 Runtime 背压。既有失败保源不能改成静默丢弃。先修确定的容器/扫描成本，再测实际 CPU/延迟，不换 GC 打分算法。

## R2-07 / P2：正文有界不等于 scope/catalog/checkpoint 元数据有界

**对执行者的影响：** 即使每轮只发送少量 Token，长期工具调用和任务切换仍让 Context 的内存、扫描和检查点持续变大。

**证据：** [scope.rs](../../../crates/context-simple/src/scope.rs) 134–173 每次工具 scope 新增节点，close 只标 Closed；[scope_tree.rs](../../../crates/context-simple/src/scope_tree.rs) 20–94 没有退休路径，序列化整张 scopes。materializer 108 起每轮遍历 scopes；GC `task_completed`（full/mod.rs 1133–1140）又会按条目扫描 scope 树。ExternalMap 与派生 Catalog 继续保存所有外置条目的元数据；Cold→External 只改驻留枚举，并非把索引完全移出内存。

**归属：CTX-9。** 对已关闭、无当前引用的作用域和历史索引定义有界生命周期/既有 store 的按需恢复路径，保留活跃祖先、终态身份、引用闭包和保留 checkpoint。删除 closed task scope 时必须保留“该任务已完成”的判定依据，否则 task_completed 会变 false、错误自动召回完成任务。不能只增加几个计数器就宣称长期有界。

## R2-08 / P1：GC/检查点维护仍占 Actor 命令处理路径

**对执行者的影响：** 模型已结束后仍可能长时间卡在 GC/保存，取消与停止无响应；更小的模型 timeout 不能解决这个等待点。

**证据：** [turn.rs](../../../crates/agent-runtime/src/actor/turn.rs) 2485–2499 在维护 completion 的处理器内 await full GC；显式 collect 的 588–594 同样如此。[lifecycle.rs](../../../crates/agent-runtime/src/actor/lifecycle.rs) 348、390–395 在完成边界 await GC、保留根枚举与 storage GC。[safepoint.rs](../../../crates/agent-runtime/src/actor/safepoint.rs) 99–104 在 checkpoint assembly 内 await Context maintain，Rolling 此时可再次调用 compactor。这些等待发生在 Actor select 的处理分支内，修好的 Materialize lane 未覆盖它们。

**归属：EXEC-7。** 对已定位的边界复用现有 operation/continuation lane，保留前后提交事实与 failure fencing。checkpoint 维护的取消也必须清楚；不能提前宣布 TurnCompleted/可恢复。物理删除不能因为取消而伪称回滚，必须保留已执行结果和原先的引用保护证明。

## R2-09 / P2：完成记录移出热表后，没有产品级冷查询接续

**对执行者的影响：** 磁盘上可能仍有旧日志/产物，但 UI/SDK 按任务 ID 查询会得到 task not found；“旧事实在 journal”尚不等于可审阅。

**证据：** [task.rs](../../../crates/agent-runtime/src/task.rs) 2088/2092 移除旧 task/completion；[commands.rs](../../../crates/agent-runtime/src/actor/commands.rs) 382–400 的 TaskDetail 只查询内存表，没有已有记录的有界回读分支。已新增测试只验证近窗口可查、早期 `completion_of` 为空（task.rs 2721–2744）。

**归属：EXEC-8。** 在既有 journal/artifact 上接通有界只读查询与历史定位，说明可查询/已过保留期/缺失/损坏，保持原 task/run/digest。查询不能触发模型、工具或恢复副作用。该项与 R2-01 不同：前者是校验不合法，此项是产品查询链不完整。

## R2-10 / P2：cache miss 被错误标成 cache write，正常 Responses 仍丢 write 字段

**对执行者的影响：** 供应商用量被赋予错误语义，后续缓存归因和费用计算会失真。

**证据：** [sse.rs](../../../crates/provider-openai/src/sse.rs) 287–292 直接将 `prompt_cache_miss_tokens` 填入 `cache_write_input_tokens`；注释也将 miss 解释为写入。DeepSeek 官方定义是“未命中缓存的输入”，不是独立缓存写入证据，见 [Context Caching](https://api-docs.deepseek.com/guides/kv_cache/)。[responses.rs](../../../crates/provider-openai/src/responses.rs) 227–251 则仍只读取 cached_tokens，新增 ModelUsage 的 write 字段默认 None。diagnostics 的字段读取不能替代产品 transport。

**归属：COST-6。** hit/miss/write 独立可选字段，区分 observed 与 derived；实际 write 仅来自供应商明确字段。按端点/协议/profile 确定包含关系，避免对相同输入重复计费。这里未声称已发生真实错账收费，本轮没有调用 provider。

## R2-11 / P2：维护/晚到/部分用量仍不能形成完整成本账目

**证据链：**

- Dynamic 的 [engine.rs](../../../crates/context-simple/src/engine.rs) 1360–1370 仍以 `input>0 || output>0` 决定发 compaction；失败产生的 Unknown 0/0 行消失，Rolling 已修的分支不能代替它。
- [compactor.rs](../../../crates/agent-compose/src/compactor.rs) 57–63 收到空 summary 后返回 Err，丢掉已收到的 usage；80–90 透传时还将缺失 cached-input 拍平成 0，没有 write/miss 的完整透传。
- [metrics.rs](../../../crates/agent-eval/src/metrics.rs) 915–944 统计 estimated/unknown compaction 次数，但没有像 model_used 那样同步设置全局 lower_bound，也不消费 compaction 的 retries。若只有 maintenance 有未知/失败重试，统一完整性标志仍可能读成完整。
- [tools.rs](../../../crates/agent-runtime/src/actor/tools.rs) 861–942 丢弃 stale 结果也丢其用量；业务结果必须丢，但已知费用可以按原调用身份补充。维护被取消时，usage 跟着整个 maintenance report 丢失，取消专用模型行仅覆盖 OpKind::Model。

**归属：COST-7。** 用既有事件/日志形成每个逻辑调用与 attempt 的可对账身份，费用接收与业务状态提交分离；known usage 能补充 unknown，但不得重复算一次、不得让晚到结果推进任务。全局完整性应综合 main/maintenance/retry/repair，旧事件与部分字段保留下界。

## R2-12 / P2：交互 restore 修了有界读取，启动 restore 仍经整文件 helper

**证据：** [checkpoint.rs](../../../crates/agent-runtime/src/checkpoint.rs) 427–435 的 `decode_checkpoint_file` 仍调用 `std::fs::read`。[agent-tui/session.rs](../../../crates/agent-tui/src/session.rs) 650–658 的 `load_runtime_checkpoint` 调用它，`main.rs:63/98` 在启动显式恢复与 latest 恢复使用该 wrapper。旧 E08 的交互 `/restore` 新实现不覆盖这条入口。

**归属：EXEC-4 残余。** 修共享读取边界并验证真实 startup 路径：cap+1 同句柄读，超限在分配阶段拒绝，正常完整校验保留；不再只测试另一个局部 helper。

## 需要优化/验证，但不冒充新缺陷

1. **维护预算必须能从产品入口配置。** 当前 Rolling token 阈值默认 `u64::MAX`，compose 使用默认 RollingConfig；设置超时只控制单次 transport，不能代表包含重试、多次折叠和多个维护触发的整个执行段上限。COST-8 交付可用的预算配置、预留/结算和延期说明；同源失败需避免每个触发无条件重打，保源与恢复仍可靠。
2. **摘要局部覆盖与累计承诺。** CTX-3 对单条超长来源仍截断但没有片段范围；仅一个来源时 excluded=0，不显示覆盖提示。压缩失败 fallback 裁短后，仍可能因 source_ids 包含旧卡而终结旧卡。作为 COST-5 的语义压力场景验证，若证明丢失不可替代要求则回 CTX-3 原切片修复，不另建摘要研究阶段。
3. **GC 的性能收益需要真实层面度量。** Full sweep 的线性查找、全量序列化、反复扫描 closed scopes 是可定位的开销；优先做保持结果等价的数据结构/批次修复。索引“存在”不等于所有调用都是 O(1)，限输出行数不等于限扫描/峰值。
4. **默认产品仍是 Rolling。** Dynamic 的 GC 修复通过并不自动覆盖默认体验；最后同时注明各 profile 的实际入口、语义保证、调用成本和未验收项。
5. **已存在的 safepoint 测试失败必须由集成者复核。** 前序回执反复记录 `failed_checkpoint_write_fences_continuation_until_a_retry_lands`。本轮没有重跑，也没有把它归因为 R2-01；一个旧失败不能用别的 target 通过抵消。

## 阶段验收

费用主指标保持“固定质量与恢复要求下每个成功任务的全成本”，同时记录失败样本和所有 unknown。供应商缓存能力以实际端点/模型为准，[OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)的稳定前缀、显式断点和计费规则不能泛化到兼容网关。缓存命中不担任证据正确性判据。

联合旅程包含：超过热窗口的真实完成—保存—冷恢复、长工具序列/多 episode、Context store 长时间失败后恢复、受保护正文跨 TTL、取消发生在 GC/保存/维护中、旧产物有界查询、同源维护失败与费用未知。用现有 compose/runtime/provider fixtures 和既有 COST-5 跑通，不新增评测框架、不改冻结 M15 证据。

当前支持声明只到源码确认的实现；新问题、运行复现、统一当前树 CI、真实 provider 费用对照均没有在本轮完成。详见 [EVIDENCE.md](EVIDENCE.md)。
