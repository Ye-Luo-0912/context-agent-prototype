# M18：可持续、低成本的单 Agent 执行——三部分任务书

> 本文件保留第一轮任务及验收要求。当前工作树已有相应实现，第二轮实际开工与残余见 [第二轮报告](../2026-09-12-gc-core-followup/REPORT.md) 及其 A/B/C 三份任务；唯一活动队列以 NEXT_TASKS 顶部为准。

用户结果：用户持续交给 Agent 仓库任务，追加条件、停止、重启后仍能依靠正确证据推进；在相同产物质量和恢复约束下，每个成功任务的实际成本下降，资源与失败处理有界。

本任务书由 2026-09-12 源码审查产生。它安排后续执行，不表示实现已完成。审查基线是 `685b6bbb` 加已有工作树修改，证据见 [REPORT.md](REPORT.md) 和 [SOURCE_MANIFEST.json](SOURCE_MANIFEST.json)。开工先读 `docs/CURRENT.md` / `docs/NEXT_TASKS.md` 当前段，执行 `git status --short` / `git rev-parse HEAD`，再复核本片实际实现及调用方。不要恢复成 HEAD 覆盖用户修改。

## 阶段边界与所有权

| 部分 | 执行负责人拥有的范围 | 需要对方提供的接口 |
|---|---|---|
| A：上下文与长期记忆（CTX） | context-simple / context-baselines；runtime/prompt.rs 的证据渲染与覆盖；正文缓存覆盖语义 | 最终可见清单、材料化取消/提交协议 |
| B：执行核心与恢复（EXEC） | RuntimeActor、TaskManager/Core 接缝、workspace/storage/host、平台与 .NET 恢复消费面 | A 的准备阶段语义；C 的用量事件需求 |
| C：缓存与成本（COST） | provider-openai、compactor、usage 聚合、成本诊断与 GUI 成本消费面 | Actor 发射点和共享 usage 契约；A 的最终请求派生结果 |

**共享接口单一维护者为 B。** `agent-contracts`、`agent-platform-protocol`、`agent-runtime/command.rs`、compose 入口、actor 发射点、.NET 公共 DTO/fixture 都由 B 按 A/C 的短接口说明合入，领域语义由提出方确认。C 不直接抢改 A 的 prompt.rs；先提交输入/输出形状与反例，由 A 合入相关小变更。GUI 成本面由 C，其他恢复/结果面由 B；同一个 ViewModel 文件不得双写，先预约窗口。

可以使用隔离分支/工作树，但当前大量修复未提交：不能从裸 HEAD 开工后假定这些修复已经存在。由集成人员先保留并形成可复现基线，或明确采用经核验的工作树快照。不得擅自 reset、清理 `.trae/` / `.workbuddy/`、提交他人代码。

RuntimeActor 仍是唯一编排者；Core 仍拥有审批/effect/提交恢复；Context 可替换；缓存仅是派生优化。保留 OperatorClosureOnly 与 EvidenceRequired 区分。不得以缩短回合为由改变完成权、权限、tool-result 配对、失效证据、GC 终态或重放副作用。

## 第 A 部分：让执行者始终有正确、可找回的依据

### CTX-1（首片，P1）最终可见覆盖清单；承接 CORE-1/E01

**用户能做什么：** 跨窗口读取和继续任务时，模型不会因为“文件版本一样”漏掉尚未展示的片段。

- 入口：`agent-runtime/src/prompt.rs`、`execution/body_cache.rs`、`context-simple/src/materializer.rs`、`agent-contracts/src/context.rs`。
- 去掉未知范围经 legacy identity fallback 获得覆盖权的通路；旧数据可解码，但未知保持未知。由最终实际回注/保留的正文派生 identity＋extent＋complete＋provenance 清单，定价、渲染省略和消费 ACK 用同一依据；清单不进入第二套持久状态。
- 必要回归：所有窗口均未知；只有一个无关文件有窗口；同版本不相交/包含窗口；裁剪窗口；缓存命中但未回注；旧 checkpoint；required 正文被最终裁掉。分别核对最终 ModelRequest 正文与 ACK，不能只断言 helper 返回值。
- 定向检查：`cargo test -p agent-runtime --lib prompt::`，对应 `context-simple` required/consumption 回归，契约新增字段的兼容测试；只在实际改动涉及的 target 上运行。
- 停止条件：未知不省略、明确覆盖仍去重、预算与消费一致。不扩大缓存容量，不引入新 Frame 编译器。

### CTX-2（P1）明确撤销同一个要求；承接 CORE-2/E02

**用户能做什么：** 改日志格式不会忘掉原有超时/安全/兼容要求。

- 入口：`context-simple/src/gc/reachability.rs`、用户 ingest 与 `tests/lifecycle.rs`。
- 删除 `instead/revert + 同实体` 的整行捷径；定义可解释的明确替代对象，不能用“共享任意实词”给语义死亡授权。用户明确引用某个要求或精确指令关系才能终结；歧义先并存。存储摘要不足以证明替换时，不以摘要前缀作终态依据。
- 回归：REPORT E02 的双消息；一句包含多条要求；“不要替换/仍然保留”；引用/条件句；中文追加条件；真正撤销仍成功；跨 task 不影响；Stored/Warm 与 Resident 一致。
- 定向检查：context-simple lifecycle/GC 相关测试，必要的 restore 测试。
- 停止条件：既有合法明确撤销保留，反例旧要求仍可材料化/召回。不重调全局打分、不引入 LLM 裁判替代状态权威。

### CTX-3（P2）摘要来源、覆盖与失败身份；E04

**用户能做什么：** 长任务切换阶段后，工作笔记指出自己真正涵盖哪些内容，遗漏仍有原始引用可找回。

- 入口：`context-simple/src/distill.rs`、`engine.rs::run_distill`、`tests/distill.rs`；需要变更压缩接口时与 COST-4 共用一次契约变更。
- 在装入有界输入的同时生成来源片段清单，只有实际贡献的来源进入 DerivedFrom；记录截断而不伪造完整覆盖。先定义 episode 卡是局部笔记还是累计笔记，然后按该语义处理旧卡；旧卡退役需有覆盖/替代依据。失败 fallback 明示为未完成压缩，原始内容与未解决要求继续可检索。
- 回归：第一条来源超过 2000 字符、后条有唯一约束；8/9 条来源；三次 episode 旋转；旧卡有本轮没有重述的 open loop；空输出、失败、取消；恢复后 provenance 不膨胀。
- 停止条件：来源声明不大于实际输入，跨阶段需要的信息有可证明的保留/检索路径。无需要求摘要逐字保留所有原文。

### CTX-4（产品增量）默认长期上下文能力与可行动的缺失；D01

**用户能做什么：** 默认启动后能理解哪些依据缺失、如何补齐，并在继续/恢复后持续处理真正必需的约束。

- 入口：`agent-compose::build_context_engine`、host/TUI 默认配置、两类引擎 required miss、runtime 的降级呈现。
- 第一项交付是实际 profile 决策：明确 production 要求的最低能力，再在“补齐 Rolling 最低义务”与“通过验收后切换 Dynamic”中选一条。保留实验 baseline 的可比语义，不直接全改。将不可满足/预算不足/暂时不可读取区分为模型可理解的有界状态，给出可用恢复动作；不在 required 缺失时伪造 settled。
- 验收：从真实默认 host 入口跑“新增约束→跨文件编辑→预算让出→继续→冷恢复”；录下实际 profile、最终模型请求、required misses 与恢复结果。证明的是选定产品 profile，未覆盖的 profile 明说。
- 停止条件：默认入口与声明一致，必要证据闭合或明确让出给操作员。不是无限自动重试 required miss。

## 第 B 部分：让执行、取消和长期恢复保持有界

### EXEC-1（首片，P1）材料化等待可取消；承接 W04/E03

**用户能做什么：** 上下文读取慢时仍能取消任务，得到可信的取消或恢复受阻结果。

- 入口：`actor/model.rs` 维护续体、`actor/maintenance.rs`、`actor/tools.rs`、`services.rs`、Context materialize/ACK 协议。
- 先复现：真实 SimpleContextEngine，阻塞其存储依赖，使 Actor 停在材料化，再发 cancel/stop；保留一个可替换 gated engine 用于契约验证。
- 把不可控等待纳入现有 operation lane，Actor 只计划/核对代际/提交；准备阶段不再占住命令分支。定义 preview 写入、取消、abort/join、失败 rollback/fence 和晚到结果的处理。旧 operation 未确认停止时不能声明可信停止或接纳新状态。
- 回归：材料化永久等待、材料化完成与取消同到、取消后继续、stop、ACK 失败、存储失败、旧结果晚到；验证完整 directive 与已落地副作用保持。
- 定向检查：runtime actor/turn 对应 target、context consumption/materialize 测试；受影响的 compose 恢复测试。使用现有取消清理上限作为 watchdog，不另设靠 sleep 猜时序的测试。
- 停止条件：命令接收不被材料化等待阻塞，取消确认在既有清理边界内且状态正确。其他没有证实阻塞的异步路径不一起大重构。

### EXEC-2（P2）完成任务的热投影有界；承接 N7/E06

**用户能做什么：** 同一宿主持久使用、完成很多任务后，查询和 checkpoint 仍能持续工作，旧结果可审阅。

- 入口：`TaskManager`、`TaskManagerSnapshot`、checkpoint capture/restore、task_detail/list 的只读消费面。
- 给完成任务定义有界热驻留与既有持久记录的引用式访问；保留 active/suspended/未决 effect、结果证明与 checkpoint 恢复根。不要简单 drain tasks/completed 导致权威消失。容量无法安全收敛时在接纳前明确背压。
- 回归：顺序创建并经合法路径完成至少 1000 个小任务；穿插未完成任务、失败/取消和恢复；同一活动任务不会被挤掉；旧结果可查；checkpoint 和内存计数不随完成任务无限增长。将阈值、计数和局部峰值记录到现有诊断，不能只观察某个 Vec。
- 停止条件：热对象/快照有已说明的硬预算；磁盘保留与引用规则明确。不是新建 run catalog 数据库或自动删除用户结果。

### EXEC-3（P2）恢复后的活跃 artifact 引用受保护；承接 CORE-3/E07

**用户能做什么：** 同一任务多次冷恢复后仍能继续分页读取原始快照。

- 入口：`workspace/lib.rs` artifact lineage、`actor/restore.rs` finalize、正式 checkpoint 的引用集合与平台状态。
- 以活跃 sealed artifact/恢复根定义必要引用集；保持承载有界，明确容量拒绝/降级，不只保留最近 32 代。处理恢复提交后登记失败的可见状态，模型/SDK 能区分“恢复任务成功”与“所需证据可继续读取”。
- 回归：33/64 次恢复仍使用首代快照；原文件后来变化；谱系丢失、损坏、写失败、超界；外来 run 引用拒绝；同 task 完成证明仍走严格身份校验。
- 停止条件：活跃引用保留或阻塞明确，分页不换版本，不增加副作用重放授权。

### EXEC-4（P2）长状态载入与审阅在读入阶段有界；E08

**用户能做什么：** 载入大/损坏状态或审阅长 trace 时得到结构化结果或拒绝，不先吃下整个文件。

- 入口：TUI `/restore`、workspace lineage reader、replay `run_summaries_from_files`；优先复用既有 bounded reader，不新建 I/O 框架。
- 同一个已打开文件句柄做 cap+1 读取；trace 按行增量折叠，结果集合本身也有预算与 omitted。文件读完后检查长度不算实现该功能。
- 回归：恰好上限/超限一字节、多字节 UTF-8、元数据检查后文件增长、超长单行、长 trace 多任务；正常完整 checkpoint 仍按现有摘要验证。
- 停止条件：三条已确认入口闭合；未证明有问题的全部文件 API 不迁移。

## 第 C 部分：在同质量下实测降低全任务成本

### COST-1（首片，P2）完整调用账本；承接 CORE-4/E05

**用户能做什么：** 看懂一次任务到底用了多少主模型/维护/修复调用，以及哪些费用未知。

- 入口：`ModelUsage` / `CompactionOutput` / `ContextCompacted`、retry、Actor terminal 发射点、eval metrics、SDK/GUI 成本投影。
- 先交 B 一份最小共享字段与兼容 fixture：run/task/turn/operation 或逻辑调用身份、attempt、role、terminal status、逐字段可选 usage、来源身份与完整性。复用现有事件/journal，不开账单数据库。
- 模型启动后成功/失败/取消都有可对账终结；maintenance 失败也有 unknown，不使用非零 token 作为是否发生调用的判据。身份穿透 compaction→事件→汇总；重试成功保留失败尝试未知费用。保留部分已知计数，避免重复累计。
- 回归：成功、只有输入计数、没有 usage、流中断、重试后成功、重试耗尽、取消、空摘要/压缩失败、非模型脚本零、旧 JSON；多次订阅/重连缺口不伪造完整成本。
- 停止条件：账目可分角色并明确 lower-bound/unknown；GUI 显示范围（当前连接/当前 run/完整任务）与缺口。仍不把 usage 当真实货币费用。

### COST-2（P2/增量）供应商能力与可计费字段归一化；E05

**用户能做什么：** OpenAI、DeepSeek 和其他兼容环境各用真正支持的缓存能力，能解释缓存读写对成本的影响。

- 入口：provider Chat/Responses parser、diagnostics、profile；经 B 合入可选字段和 .NET fixture。
- 默认保持 ProviderDefault；显式模式需要确认 endpoint/model/protocol 的能力，禁止按域名或别名猜支持。保留 provider-native 字段的语义出处、输入/cache-read/cache-write/cache-miss/output/必要 reasoning 字段（有报告才填）。不重做已有 PromptReuseBoundary 和 ResponsesExplicit。
- 每种能力用 loopback wire/usage fixture，断言默认不发扩展字段，显式模式失配在发送前拒绝，普通/observed 发送内容一致。供应商计费分桶不能重叠求和。费率/profile 绑定来源、模型版本和日期，未知价格不报货币节省。
- 真 provider 只在后续执行明确选定环境与请求/输出/费用上限后做有界验证；本任务书不授权现在调用付费模型。DeepSeek Flash 请求保持实际配置与官方标识，不擅自换模型。
- 停止条件：支持矩阵、fixture 和实际 profile 可核查，未验证网关路由标 UNKNOWN。

### COST-3（增量）最终请求稳定复用与组装开销；D03/D04/D05

**用户能做什么：** 相同有效证据的后续轮次减少不必要的前缀失效与重复序列化。

- 依赖 CTX-1、COST-1/2；provider 侧归 C，prompt 侧由 A 合入。
- 把变化的诊断计数移出稳定证据前缀或省略；相同合法工具集合维持规范顺序；复用本轮最终派生清单与层计费，减少每次删 item 后全请求 clone/hash。最终完整消息、角色、工具定义、可调用权限与结果配对必须逐项等价。
- 若需多个稳定块/显式断点，先从现有边界演进，绑定准确模型/工具/内容修订；不能缓存跨权限变化结果、冻结旧正文、保留已失效证据、填充无用 Token 或无限追加历史。
- 先离线记录首个变化段/复用边界/消息工具摘要，再用 COST-5 同起点对照。稳定字节只是诊断，不是缓存收益。
- 停止条件：至少一个已测瓶颈得到改善，语义 fixture 全部保持。没有收益则不默认启用，报告负结果。

### COST-4（增量）维护调用独立预算与有效压缩；D02

**用户能做什么：** 长会话保持记忆能力，但不会为短摘要反复支付主模型的大输出和重试预算。

- 与 CTX-3 协调压缩契约；入口为 compose compactor/profile 与 Rolling/Dynamic 的既有触发点。
- 增加可选 maintenance profile，显式输出上限/超时/重试与每个执行段的累计维护预算；默认行为迁移需记录。相同源身份的并发/重试工作只在结果可证明等价时复用，变化的指令或未闭合约束必须失效。失败回退保源，不能虚报已压缩或把维护关掉导致内存增长。
- 先做同模型更小输出预算，再比较经核准的轻量模型；模型切换是质量决策。使用有效约束保留率、恢复/补读成功、维护成本与节省的后续输入量衡量，不以摘要短为唯一指标。
- 回归：独立预算真正到 wire；取消仍贯通；连续失败不会每个 BeforeModel 无界重试；累计维护额度耗尽时明确让出/延期；与 CTX-3 的来源覆盖共用测试。
- 停止条件：质量不退化并且摊销成本下降，或者保留原默认并报告不通过。禁止静默用便宜模型替换已配置主模型。

### COST-5（阶段验收）固定质量的长任务成本对照；承接 W 系列第 7 行

**用户能做什么：** 得到可重复的结论：真实长期任务是否更便宜、仍然正确且可恢复。

- 复用现有 `agent-compose` 真实组合走查与 `agent-eval` / `kv_cache_walk` 诊断，不建立新评测框架，不改 M15 冻结证据。
- 三类任务：小缺陷修复；跨 crate 多文件变更并保留用户已有修改；长任务跨 episode、追加约束、取消/冷恢复/补读。每类先做至少 3 组独立配对起点；模型/端点/权限/源树/验收脚本一致，缓存 arms 隔离预热并交替顺序。样本小就明确仅为初步证据，不声称普遍统计优势。
- 一次只改变一个优化：前缀稳定化、maintenance profile 等分别归因，最后再组合。每个实验预先记录请求次数/输出/时间/费用上限，超限停止；失败和中断保留在报告分母内，unknown 不计成零。
- 必填指标：产物正确率与验收覆盖、必需正文 miss、重复读/验证/repair、主/维护/重试 Token 与费用、cache read/write/miss、未知账目、p50/p95 时间、取消确认、热对象/RSS 峰值、checkpoint/trace/store 字节、恢复后的任务与 artifact 身份。
- **正式通过条件：** 固定功能/权限/完成语义全部通过，当前默认产品入口跑通，基线与方案成本账目完整且同口径，成功任务全成本确实降低；取消与恢复不退化；内存/热状态有界。只有 Token 降低但计费无法核实时，只声明 Token 优化；真实 provider 未跑写 NOT_RUN，账目不全不能宣称降本通过。
- 目标建议：先冻结可复现基线，再设费用降低目标；不预先承诺 30%/50% 等无证据收益。达到上述验收即停止扩展。

## 执行次序与集成

| 波次 | A 上下文 | B 执行 | C 成本 |
|---|---|---|---|
| 开工 | CTX-1 → CTX-2 | EXEC-1；共享字段/fixture 单一合入 | COST-1；先在可独立文件与 fixture 工作 |
| 形成长期闭环 | CTX-3 | EXEC-2 → EXEC-3 | COST-2 → COST-4（与 CTX-3 接口同步） |
| 优化与验收 | CTX-4；按预约合入 COST-3 prompt 改动 | EXEC-4；集成默认产品链 | COST-3 → COST-5 |

箭头只规定本线顺序，不要求等其他线全部完成。执行者一次只交付一个切片；需要跨线接口先给具体字段、所有者和失败语义，不直接覆盖共享文件。成本实验必须等参与方案的正确性与账目闭合，界面/离线 fixture 工作可以提前。

集成沿用既有 CI，开发只跑本片相关检查；涉及共享契约时补编译/conformance 与双侧 fixture。每片回执写：用户能做什么、代码落点、默认入口是否启用、实际运行命令与结果、是否真实 provider、残余限制、下一片。未运行的命令不得写通过。只有文档变化时运行 `python scripts/doc_consistency.py`。

前序已落地的 GUI 入口/era、exact-request 查询、artifact 分页、changes/context 读模型、连接契约、usage 消费保留；CURRENT 中缺失的人工/真实 provider/远端 CI 证据随相关路径验收补齐。不要整套重开 CORE/PLATFORM/GUI，不把所有历史 backlog 变成 M18 必需。
