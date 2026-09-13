# 执行者 LLM 视角全仓审查与下一阶段安排

审查日期：2026-09-12。基线：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d` **加审查时已有的未提交工作树修改**。本轮只审查、分流和编排；没有修改产品实现、运行 Rust/.NET 测试、调用付费模型、提交或发布。前序实现回执中的通过数不是本轮验证结果。

## 结论

下一大阶段建议命名为 **M18：可持续、低成本的单 Agent 执行**，分为上下文与长期记忆、执行核心与恢复、缓存与成本三部分。平台与 GUI 消费面归到相应切片，不继续作为独立扩展界面的阶段。M17/9 月 11 日工作树的集成与真实使用验收仍需完成，不能因命名 M18 就宣称上一阶段已正式验收。

已有的 Actor/Core 权责、可替换 ContextEngine、过程监督、恢复事务、有界工具输出、工具发现、正文缓存和供应商缓存边界值得保留。下一步的重点是让**最终请求的证据、状态转移、可恢复引用和完整调用成本相互对齐**。扩大上下文窗口或缓存容量本身不能证明长期可靠或费用下降。

本轮列出 **8 项源码可定位的问题：3 项 P1、5 项 P2**。证据等级均为源码链路与可构造反例；没有把未执行的反例标为运行复现。优化与未验收项单列，不能当作已证实缺陷。

## 覆盖与证据边界

- 盘点了 20 个 Rust workspace crate、Avalonia 桌面、.NET SDK/测试和构建发布入口，共 **417 个源码/测试/构建文件，281,371 行**。包含 Cargo 清单；不含生成文件、冻结实验输出、seed/golden/suite 仓库和 JSON fixture 数据文件。协议 fixture 的存在性通过前序变更与对应测试入口核对，未逐个重放。
- 对盘点文件全部进行词法结构扫描；对下面的关键路径逐段追踪实现、调用方及相关测试入口。**这不是 281,371 行逐行语义审查，也不是全仓无缺陷证明。** `await`、整文件读、集合增长等扫描命中包含测试和注释，不能直接算漏洞。
- [SOURCE_MANIFEST.json](SOURCE_MANIFEST.json) 固定每个源码文件的内容摘要；[STRUCTURAL_SCAN.json](STRUCTURAL_SCAN.json) 记录全盘扫描；[READ_RANGES.jsonl](READ_RANGES.jsonl) 记录辅助阅读器请求的范围。早期 shell 直接读取不在范围日志内；部分工具输出有截断，因此日志是读取请求记录，不是逐行读完证书。下面结论的关键分支另作了窄范围复读。
- [SOURCE_DRIFT.json](SOURCE_DRIFT.json) 记录审查快照后的源码漂移检查。本轮的源码结论绑定该工作树，不能仅凭 HEAD SHA 复用到别的执行者工作树。

| 模块 | 本轮着重追踪的边界 | 结论归属 |
|---|---|---|
| agent-contracts | ModelInput、覆盖窗口、usage、压缩事件、预算 | E01、E05；保留类型化契约 |
| context-simple | 材料化、必需正文、GC 根、语义终态、蒸馏、store、checkpoint | E01、E02、E04；不重写 GC 算法 |
| context-baselines | rolling 折叠/失败回退、required miss、摘要连续性 | E05；默认产品能力见 D01 |
| context-contextcore / agent-context-service | 串行请求、超时、帧与 JSON 预算、材料化转发 | E03 的替换引擎路径；边界检查保留 |
| agent-runtime | 最终请求、surface、取消、恢复、任务表、消费 ACK、平台门面 | E01、E03、E05、E06、E07 |
| provider-openai | Chat/Responses wire、显式缓存、SSE usage、重试与诊断 | E05；COST 线 |
| agent-core | 审批、operation 接纳/提交/取消、持久 epoch | 未提出新的权限绕过结论；保持唯一 effect 权威 |
| agent-storage | WAL 上限与发布、事件 journal 队列/flush | E06 的整体资源预算；不把持久历史当缓存随意删 |
| agent-workspace | sealed artifact 与恢复谱系、路径/读取边界、日志 | E07、E08 |
| tool-runtime | 搜索覆盖正文、patch 失败清理、Git 隔离、验证监督入口 | 前序修复保留；测试入口核对，未重新验收所有工具 |
| agent-process / agent-capability-process | reap 确认、MCP 写/读取消和 poisoned session | 已有实现不重做；真实跨平台行为未重跑 |
| agent-compose / agent-host | 默认引擎、compactor 共用模型、provider profile、公开路由 | D01、COST-2/4、集成验收 |
| agent-tui / agent-replay | continue/restore 和大 trace 审阅读取 | E08；非模型回放不冒充 live |
| agent-eval / agent-conformance | 成本聚合、既有评测入口、依赖矩阵 | E05、COST-5；不改冻结实验 |
| .NET SDK / Agent.Desktop | 重连水位、未知结果、typed usage、结果分页 | 前序工作不重做；成本面板需完整性与范围说明 |
| CI / dist | 双系统 Rust 检查、定向宿主检查、来源文件与打包入口 | 本轮只读配置；不复述前序 SHA 的 CI 为当前通过 |

## 问题清单

### E01 / P1：范围未知的正文仍能走 identity-only 分支省略历史正文

**用户影响：** LLM 手里只收到同一文件版本的一部分文字，却可能失去另一段已选正文。

**源码链路：** [prompt.rs](../../../crates/agent-runtime/src/prompt.rs) 557–568 把所有回注行加入 `visible_body_identities`；583–602 对 `window=None` 不加入窗口；1190–1222 在窗口集合为空时退回 `visible_body_identities_cover`。[materializer.rs](../../../crates/context-simple/src/materializer.rs) 837–866 也有相同的定价退路。[body_cache.rs](../../../crates/agent-runtime/src/execution/body_cache.rs) 76–105、149–166 接受并传递未知窗口。

**反例：** 旧 frame/缓存行携带 `src/a.rs@r1` 与片段，但没有可信范围；最终请求中没有其他窗口。历史材料化有同版本另一片段。窗口集合为空，identity 命中会使历史片段变空。这不等于当前正常 fs.read 的有范围分支有错；触发面是兼容/范围缺失路径。

**安排：CTX-1。** 兼容读取旧数据可以保留，未知范围不能兼容成整文件覆盖。所有定价、最终渲染与消费事实使用同一份最终可见清单。对应前序 CORE-1/F01 的残余，不重做已正确的区间包含修复。

### E02 / P1：`instead` / `revert` 仍能跨维度终结同文件的旧决策

**用户影响：** 修改日志方式这样的补充要求可能撤销原有超时要求；语义终态随后阻止正常召回。

**源码链路：** [reachability.rs](../../../crates/context-simple/src/gc/reachability.rs) 171–192 调用 `has_whole_entity_cue`，其中 229–236 将“出现 instead/revert 且共享实体”直接判为整条替换。328–391 对同任务内 live 的旧决策排入 supersession。[engine.rs](../../../crates/context-simple/src/engine.rs) 934–953 将用户决策消息接入该路径。

**反例：** 先输入 `use AuthService.rs with a 5-second timeout`，后输入 `use AuthService.rs with structured logging instead of plain-text logging`。新消息指向日志维度，却满足 `instead`＋共享文件实体；旧超时决策会被排入终态。现有 `a_replace_cue_on_a_shared_file_does_not_withdraw_an_unrelated_requirement` 覆盖 `replace ... logging`，没有消除这个捷径。

**安排：CTX-2。** 使用明确目标/完整命题的替代关系；无法证明同一要求时保留两条。补 instead/revert、否定/引用、多约束同消息、中文补充的反例。不能继续叠通用同词启发式并把它叫身份验证。承接 CORE-2/F02。

### E03 / P1：材料化仍占用 Actor，取消无法及时受理

**用户影响：** 慢外存或慢 ContextEngine 出现时，看起来还没开始模型请求，取消与状态命令已经被一起阻塞。

**源码链路：** [actor/mod.rs](../../../crates/agent-runtime/src/actor/mod.rs) 1601–1624 在 select 分支内 await 整个 completion handler；[tools.rs](../../../crates/agent-runtime/src/actor/tools.rs) 949 await 维护续体；[model.rs](../../../crates/agent-runtime/src/actor/model.rs) 879–897 在续体内直接 await `context_materialize`；[services.rs](../../../crates/agent-runtime/src/services.rs) 583–587 直接转发。[engine.rs](../../../crates/context-simple/src/engine.rs) 1510–1546 会等待 gate 与外存读取，[store.rs](../../../crates/context-simple/src/store.rs) 211–227 的字节上限不提供时间上限。

**反例：** 在材料化 await 上用 gated engine 停住，随后发送 Cancel/Stop；Actor 尚未回到 select，不能处理它们。现有维护/用户 ingest 的取消修复不是这一边界的覆盖。

**安排：EXEC-1。** 扩展已有 operation/准备阶段，计划与提交仍由 Actor 拥有；等待工作可取消、有期限、abort/join 和 generation fencing 明确。材料化会写 pending preview，不能简单丢 Future 后当作未发生。不要加新 worker 调度器。先用真实 SimpleContextEngine＋可阻塞存储依赖复现，再保留替换引擎契约测试。

### E04 / P2：episode 蒸馏来源集合大于实际输入，替代旧摘要也未绑定覆盖

**用户影响：** 摘要的来源关系可能指向压缩器没有读到的条目；跨 episode 的工作笔记可能缺少以前未闭合的事实。

**源码链路：** [distill.rs](../../../crates/context-simple/src/distill.rs) 68–76 先收集最多 8 个 ID、拼接正文，再统一截到 `COMPACTION_SOURCE_CHARS=2000`；164–168 为全部 ID 建 `DerivedFrom`。196 起按 task 终结旧 episode 卡，而本次输入排除了旧 episode 卡（50–54），没有证明新卡覆盖旧卡的内容。[engine.rs](../../../crates/context-simple/src/engine.rs) 746–774 压缩失败会生成有界 fallback。

**反例：** 第一个来源已占 2000 字符，后面来源仍被写进 `source_ids`；新卡只覆盖当前 episode，旧卡仍被无条件 supersede。原始正文没有因此被直接删除，所以这里不宣称原始历史全部丢失。

**安排：CTX-3。** 在构造实际输入时同步构造条目/片段覆盖，区分压缩成功与 fallback，旧卡的替代必须有显式覆盖或保留可检索的分段身份。不新增语义摘要权威，不用增大 MAX_DISTILL_SOURCES 掩盖。

### E05 / P2：成本账目仍丢调用、丢身份、丢供应商字段

**用户影响：** 当前累计 Token 不足以作为完整执行账单，优化可能把未知费用当成节省。

四个已经定位的断点：

1. 普通模型失败：[tools.rs](../../../crates/agent-runtime/src/actor/tools.rs) 1499–1515 只写 Failure 后收尾；[lifecycle.rs](../../../crates/agent-runtime/src/actor/lifecycle.rs) 524–539 不补 usage。取消专用 unknown 行存在，但不能覆盖普通流中断/协议失败等终止路径。
2. rolling 压缩失败：[rolling.rs](../../../crates/context-baselines/src/rolling.rs) 472–477 直接 break，没有调用账目；483–490 只在 token 数非零时写 compaction，未知零不能可靠进入统计。失败回退保源本身是正确的。
3. 身份在事件投影丢失：[context.rs](../../../crates/agent-contracts/src/context.rs) 2358–2370 的 `ContextCompaction.usage_identity` 已存在，但 [event.rs](../../../crates/agent-contracts/src/event.rs) 216–221、786–791 的 `ContextCompacted` 没有该字段；[metrics.rs](../../../crates/agent-eval/src/metrics.rs) 898–926 直接累加。因此前序“eval 仅 observed 汇总”的描述对模型轮成立，对这条压缩链路不成立。
4. 正常 `ModelUsage` 只有输入/输出/cache-read/attempts/retries（[model.rs](../../../crates/agent-contracts/src/model.rs) 685–704）；Chat 的 [sse.rs](../../../crates/provider-openai/src/sse.rs) 146–157、267–276 不映射 DeepSeek 顶层 hit/miss；Responses 的 [responses.rs](../../../crates/provider-openai/src/responses.rs) 227–251 不保留 cache-write。独立 diagnostics 能看到部分字段，不代表正常产品账目已接通。压缩输出还丢掉主 transport 的 attempts/retries 与缓存计数。

SDK/GUI 已能区别 model_used 的 observed/estimated/unknown，并在重试日志里说明下界，应保留。但 GUI 的合计仅累计接收到的模型事件，未含压缩，重连历史也不是全量账单；不能标为整个任务完整成本。

**安排：COST-1/2。** 完成调用的角色、逻辑调用/尝试身份、终止原因、usage 完整性与可选计费字段贯通；partial observed 保留已知字段，不整行丢失。缓存读写互相是否包含由供应商契约决定。普通失败、取消、压缩失败、重试成功、旧事件与重连缺口都要诚实。

### E06 / P2：已完成任务持续增长，热状态和 checkpoint 没有对应生命周期

**用户影响：** 单宿主长期完成许多任务后，查询/快照越来越重，最终可能撞上 checkpoint 的 16 MiB payload 上限。

**源码链路：** [task.rs](../../../crates/agent-runtime/src/task.rs) 1360–1365 的 tasks/completed 为 Vec；1472–1481 的 256 上限只数非 Completed；1931 新建追加，1965–1970 完成只改状态并追加 completion。1581–1617 的 prospective snapshot 复制全表与全部完成记录。当前读取与源码搜索未发现同表的移出路径。Context GC 对完成任务的驻留根已有收敛，不能代替 Runtime 任务表的边界。

**安排：EXEC-2。** 保留活跃/可恢复任务与近期结果的有界热投影，完成事实通过已有持久记录/只读查询继续可审阅；达到容量时明确背压。不得丢完成权威、重新激活终态或新建 TaskGraph/第二数据库。磁盘历史配额应显式报告，不允许为“有界”删除仍被 checkpoint 引用的证据。

### E07 / P2：按最近 32 个 run 保存恢复谱系会淘汰仍被使用的旧引用

**用户影响：** 一个长期任务反复冷恢复后，第一次捕获且仍保存在任务里的 sealed artifact/cursor 会失去读取资格。

**源码链路：** [workspace/lib.rs](../../../crates/agent-workspace/src/lib.rs) 881–889 只保留最近 `MAX_ARTIFACT_RUN_LINEAGE=32` 个 predecessor，958–963 以该列表授权旧引用。反例是连续 33 次新 run 冷恢复，并持续保留第一代引用。[actor/restore.rs](../../../crates/agent-runtime/src/actor/restore.rs) 240–261 在恢复持久提交后登记谱系，登记失败仅 Warning；当前恢复“成功”不保证所有旧读引用可用。

**安排：EXEC-3。** 以仍活跃的 sealed 引用/恢复根为保护依据设计有界承载，超出能力必须类型化降级或拒绝所需继续，不能按年代静默丢引用。保持 run/task 的证据验证边界；不能把读谱系放宽成完成证明或副作用重放权。

### E08 / P2：部分有界载入仍是先整文件读、后校验

**用户影响：** 大或损坏的状态文件可能在拒绝前已经占用大量内存；长 trace 的审阅也会整体装入。

**源码：** [agent-tui/session.rs](../../../crates/agent-tui/src/session.rs) 579–583 的 `/restore` 使用 `tokio::fs::read` 后 decode；[workspace/lib.rs](../../../crates/agent-workspace/src/lib.rs) 932–945 的 lineage 同样读完才检查 bytes；[agent-replay/run_summary.rs](../../../crates/agent-replay/src/run_summary.rs) 167–175 `read_to_string` 整个 trace。另一个 replay 主读路径已有逐行上限，不能用它替代 run_summary 路径的验证。

**安排：EXEC-4。** 恢复入口用同句柄 cap+1 读取，lineage 用小额有界读，run_summary 流式折叠并限制报告行集。正常大小行为不变，不能以截断后的 JSON 成功解码冒充完整恢复。该项是资源边界问题，不宣称已发生路径逃逸。

## 优化与产品决策，不计为已证实缺陷

**D01：默认生产引擎和长期能力对齐。** `agent-host/src/main.rs:131–134` 默认 Rolling，compose 对 rolling/dynamic 均可注入同一个模型 compactor。Rolling 现在诚实报告 PromptRequired missing，而不是实际实现该义务。应明确默认产品需要哪些能力，再选择补齐最低义务或经验收切换 profile；本轮不修改默认，也不把 baseline 全部改造成 Simple。LLM 应看到可操作的缺失原因/补读途径，不能只在日志看到降级。

**D02：压缩输出“截短”不等于生成成本已被限制。** `compactor.rs:36–65` 使用同一个 ModelTransport，无请求级独立输出上限，metadata 的 `output_char_cap` 不进入 provider 参数，回复回来才裁到 512 字符；compose 普通 profile 默认输出上限 4096，超时 120 秒并带重试。安排独立且可选的 maintenance profile，先控制输出/时间/重试与单轮总维护预算，再基于语义回归选择模型；不是直接换便宜模型或停止压缩。

**D03：稳定前缀中仍夹有易变诊断。** `prompt.rs:363–373` 在工作上下文正文前放入 total/resident/warm/stored/selected 计数；`ModelInput::into_request:490–508` 将整个 context_frame 都作为复用候选。即使正文一样，计数变化也改变此前缀。这是可定位的复用阻力，实际缓存收益尚未实测。先将可省的诊断留在事件或有界后缀，稳定必要字段；不排序重写证据语义、不冻结 Focus/GC/surface、不添加 filler 换命中。

**D04：最终 packing 重复成本与 token 误差。** `actor/model.rs:1010` 每次预算计算都 clone/序列化 messages；每丢一个 item 又重组请求，后续 `prompt_layer_costs_with_catalog` 还会重组。优化可复用本轮最终派生结果，但保持唯一真值。共享 `approx_tokens` 是启发式，不是任意模型精确 token 上限；保留最终拒绝与保守余量，并用 provider 上报做误差校准。不能通过扩大预算使失败暂时消失。

**D05：只做必要的热路径清理。** `actor/model.rs:983–987` 的无条件 `TEMP-DBG`/stderr 可归并到 COST-3 的诊断开关；不要把它升级成另一个基础设施阶段。

## 缓存与成本的正确验收方式

三类复用分别度量：本地正文缓存节省重复读取和重复注入；上下文选择/压缩降低请求总量但增加维护费；供应商 KV 缓存降低部分输入的单价和延迟。这三类的 hit 不能混用。

对供应商说明只采用本轮读取的官方页面。OpenAI 的缓存由完整渲染前缀、工具和相关设置共同决定，显式断点/缓存写入能力须按实际模型与端点确认。参见 [OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)。DeepSeek 默认缓存提供 hit/miss 用量，命中需要可匹配的已写入前缀，并非保证；其 Responses 兼容面应按文档单独核对，不能把兼容 OpenAI 协议当作支持所有缓存参数。参见 [DeepSeek Context Caching](https://api-docs.deepseek.com/guides/kv_cache/) 与 [Responses API](https://api-docs.deepseek.com/guides/responses_api/)。这些是能力依据，不是对本地网关路由、计费和授权的实测。

按供应商当前、已核对的计费分桶计算每次调用，再汇总 main/maintenance/repair/retry：

`任务成本 = Σ(互不重叠的普通输入×单价 + 缓存读×单价 + 缓存写×单价 + 输出×单价 + 已知其他费用)`

字段是否互相包含先归一化，不能再次计算 cache-write；缺失账目报告完整性或下界，不当 0。主指标是固定质量与恢复要求下**每个成功任务的全成本**，同时报告失败样本费用、Token 总量、重读率、repair 次数、p50/p95 延迟、内存峰值、checkpoint 大小与取消确认时间。不能仅报 cached/input 或本地公共字节前缀。

## 交接

唯一活动队列见 [NEXT_TASKS.md](../../NEXT_TASKS.md)，执行细节见 [TASKS.md](TASKS.md)。三条线各取一个当前切片；接口负责人合入共享契约，执行前复核相关文件摘要。前序实现不重新实现，旧回执不覆盖本轮反例。

当前未验收：本轮所有反例的运行复现、当前混合工作树的集成/CI、默认产品真实长任务、完整成本对照、反复冷恢复的持久引用。完成文档交付不关闭这些代码或验收任务。
