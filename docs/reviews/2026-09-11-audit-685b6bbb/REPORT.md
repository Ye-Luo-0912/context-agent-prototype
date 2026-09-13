# context-agent-prototype：执行者 LLM 视角审查

## 审查基线与证据边界

仓库：`Ye-Luo-0912/context-agent-prototype`。固定提交：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d`，分支 `main`，提交时间 2026-09-10 18:16:27 UTC。

已读取的源码集中在实际请求组装、上下文摄入/材料化、正文缓存、工具输出、任务与平台边界、宿主启动及桌面主 ViewModel。本报告**不宣称已经逐行审查仓库每个文件**；读取范围列在文末和 `coverage_685b6bbb.json`。未完整阅读的模块没有获得“无问题”结论。

本地克隆因网络限制失败；当前环境没有 cargo/rustc/dotnet，所以**没有执行本地 Rust/.NET 回归或真实模型试验**。以下触发情景是源码推导，需要在仓库已有测试入口中复现。远端对应提交的 CI #525（run 34513313166）结果为 success；这不是本文新反例已经被覆盖的证明。

[固定提交 CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34513313166)

## 总体判断

下一阶段重点不是再增加一层编排器、再做一次 GC，或扩大正文缓存容量，而是：

> 让实际交给模型的证据、运行时用于判定的事实、平台供人核查的事实，保持相同的身份、范围、新鲜度与完整性语义。

已有 TaskAnchor、TaskProgress、范围感知材料化、独立持久化边界、原始输出 artifact、单 RuntimeActor、类型化平台路由和正式桌面客户端，都应继续复用。新增的“最终可见证据清单”应只是本轮请求的派生视图，不是另一份任务权威或新的记忆数据库。

从执行者 LLM 的角度，每轮最重要的是：当前有效指令是什么；哪些证据真的在眼前；哪些只有引用或摘要；哪些结果不完整或过期；当前允许调用什么；还有什么必须查证才能完成。结构存在不等于这些问题已经在最终请求中回答准确。

## 问题分级

P1：可使执行者丢失有效依据、误认任务/证据状态，或阻断正常生产入口。P2：产品闭环、可观测性或维护性缺口。不是安全漏洞评级，也不是已观测事故统计。

### F01 / P1：正文缓存把部分读取升级成整文件覆盖

**路径**：`fs.read → ActiveTurn::record_protocol_body → ProtocolBodyCache → visible_body_windows_for_request → materializer/最终 prompt`。

`fs.read` 本来返回路径、文件修订、行范围和 `covers_file`。缓存写入只保留 path、digest、body，且同一路径会替换；没有保留原始读取窗口与完整性。`visible_body_windows_from_parts` 对每个恢复正文直接构造：

```rust
start_line: None,
end_line: None,
covers_file: true,
```

这不是“返回的片段完整”，而是“整个文件已经可见”，两者不等价。

**第二处不一致**：`visible_body_identities_for_request` 先经过真正的回注筛选；`visible_body_windows_for_request` 则把传入的全部缓存行直接计为可见窗口。用于材料化定价/正文省略的集合，可能比实际回注集合更大。

**第三处同类问题**：broker/runtime 进行 head/tail 限幅后，保留的旧行范围元数据未同步降低为部分覆盖；`file_read_body_windows` 又直接信任这些字段。即使缓存修正，普通工具输出被二次截断的路径也要一起检查。

**反例**：同一版本读取 `a.rs` 的 L1–100，随后读取 L501–600，跨过当轮尾部保留窗口。缓存只剩后一个片段，但上下文材料化可以把前一段省略为 descriptor，理由是“完整正文已经可见”。模型没有足够信息察觉这段证据丢失。

**修复**：缓存条目保留结构化范围、修订、裁剪状态及来源；只对实际进入最终请求的正文计算覆盖；用同一份派生清单完成定价、去重、最终裁剪和消费观测。空间不够时允许 miss/重新读取，不允许伪造 whole-file coverage。未知窗口不得当成全文。

**保留已有正确代码**：`gc/reachability.rs` 的同版本正文 supersession 已检查区间包含与 clipping。问题不是这套 GC 需要推倒，而是缓存层没有提供真实输入。

来源：[S01] [S02] [S03] [S04] [S05] [S06]。

### F02 / P1（启用 supersession 时）：文件相同，不代表同一项决策被替换

`queue_decision_supersessions` 使用同任务、替换提示词、精确实体相交来判定旧决策终结。`entities_match_exact` 实际上是任意相同路径/符号，而不是“同一个决策维度”。`engine.rs` 的真实 UserMessage 摄入会直接进入这条路径。

**反例**，同一任务先后收到：

```text
use AuthService.rs with a 5-second timeout
replace plain-text logging in AuthService.rs with structured logging
```

两条指令有同一文件实体，新指令有 `replace`，但日志格式变更并未撤销超时要求。当前规则能把旧指令排入 `Superseded`；后续它是终态，不再正常检索/复活。

这不是此前“单靠实体重叠”的老问题原样重开：加入替换提示词以后，仍缺少被替换决策本身的身份。这里终结的是上下文决策记录，并不是证明 TaskAnchor 的约束字段也被修改；若约束已有独立锚点副本，影响可能被缓解，但该 supersession 判定仍不成立。

**修复**：真正的语义终结只接受精确的目标 decision/item identity 和有来源的替换关系。无法证明时保留 Live，允许降低注意力或归档，但不能不可逆地撤销。不要以添加更多英文/中文关键词代替证据，也不必为此创建复杂本体库。

来源：[S07] [S08] [S09]。

### F03 / P1（任务使用强制正文声明时）：默认 Rolling 没有兑现 required-context 契约

宿主未指定策略时选择 Rolling。`build_context_engine` 直接实例化 Rolling，没有一层公共必需正文包装。`RollingSummaryEngine::materialize` 只使用部分普通选择 hint，未处理 anchor roots/foreground requirements，返回空 required ids、空 required misses。共享基线摄入也忽略全部 ContextDirective。

**影响**：要求必须呈现的正文可以既未出现，又没有进入 `required_misses`。运行时后续只检查引擎报告的缺失数，不能据此识别未兑现的声明。

A/B 实验允许使用不同的历史保留策略；但生产环境不应将“不支持某项义务”表达为“全部已满足”。本项不等于断言所有 Rolling 任务都会错误完成；触发点是任务确实使用了必需正文声明。

**修复选择**：在公共边界落实 mandatory claims 的检查/呈现，或者声明引擎能力并明确拒绝/报告 Unsupported。任一方案都不能把空 miss 当成满足证明。不要仅为绕过问题直接切换默认 Dynamic，再宣布闭环。

来源：[S10] [S11] [S12] [S13] [S14]。

### F04 / P2，穷尽性任务可升为 P1：有命中的不完整搜索没有在正文中声明不完整

`search.grep` 知道扫描可能因文件大小、访问错误或遍历界限而不完整；部分警告放在 summary/metadata。当有命中时，`model_content` 主要输出命中正文，缺少相同完整性告警。`TurnFrame` 发给模型的是 `model_content`，不会自动追加 summary/metadata。`fs.list` 的非空结果也有相同类别风险。

**反例**：一个可读文件找到调用点，另一个关键文件因限制没有扫描。模型看到了正常命中列表，却没有得到“本次搜索不能证明已找全”的信息。

零命中路径已有警告，不应将它说成同一个漏洞。重点是 **positive result 不等于 exhaustive result**。

**修复**：结果正文提供有界、类型化生成的 coverage header，带范围、跳过原因计数、是否还有后续页以及原始 artifact/cursor；分页和 checkpoint 必须保留同样语义。扫描不完整时不能给出已穷尽的否定结论。

来源：[S15] [S16] [S17] [S05]。

### F05 / P1：Linux GUI 默认“真实宿主”选择了 Windows Named Pipe

`MainWindowViewModel.BuildTransport()` 仅在明确选 UnixSocket 时创建 UDS，其 default（包含 RealHost）创建 NamedPipe。`NamedPipeAgentTransport.ConnectAsync` 在非 Windows 上直接抛出不支持异常。

客户端已存在平台感知的 `AgentTransports.DefaultLocal()`，但 GUI 生产入口没有走它。手动选 UDS 是可用绕路，不等于默认入口正确。

**修复**：RealHost 明确做平台分派，保留用户自定义 endpoint；回归必须经过实际 `ConnectAsync → BuildTransport`，不能只通过测试专用注入连接工厂。

来源：[S18] [S19]。

### F06 / P1：未知提交按目标文字相同消除，缺少请求身份依据

`ResolveOutstandingSubmitFromSnapshot` 先清掉 outstanding request id，再用 snapshot 中是否有相同 Goal 决定提交是否已受理。旧任务、尤其历史同名任务，不能证明这次 request id 已经受理。列表有界或目标过长时，“没有找到”也不是未受理证明。

这里**不是指责已有客户端会自动重发**；旧的“未知结果禁止自动重发”逻辑已经存在。残留问题是人工界面把未证明的状态变成了确定结论。

Runtime 已有 `client_request_id` 去重收据，但明确是最多 256 条的进程生命周期内账本，不承诺跨重启 exactly-once。这个边界不能被 GUI 文案掩盖。

**修复**：在已有 actor 提交账本上提供精确查询/结果投影，至少绑定 host/process epoch、run、client request id 和 payload identity。逐一表示 accepted、known rejected、unknown、expired；跨重启无持久证据时继续显示 unknown。是否新增持久化收据应作为明确产品承诺，不能靠扩容进程内 HashMap 假装实现。

来源：[S18] [S20]。

### F07 / P2：GUI 异步读取的错误路径可以覆盖新连接的面板

Task detail/artifact/context/changes 等读取成功后检查 generation 和 connection，但部分 catch 路径会直接写“不可用”。旧连接上的失败较晚返回时，可以覆盖新连接刚取得的有效面板状态。任务详情还应绑定被请求的 task id/选择代次，而不仅是连接代次。

**修复**：每个只读请求捕获 connection epoch、选择身份、请求序号；成功、失败、finally 中的可见状态更新都受同样 fencing。最好下沉为客户端/VM 的小型统一助手，而不是再复制数套布尔状态。

来源：[S18]。

### F08 / P2 功能缺口：平台 artifact 读取只有前缀，没有读完它的路径

`WorkControlRouter::artifact` 从文件开头读 `max_bytes`，返回诚实的 `truncated`，但接口没有 offset/cursor。GUI 当前也只做有界首次读取。大文件后半段无法通过这条正式审阅链路查看。

这是 B3 有界预览之后的新产品增量，不是已完成的工具侧 artifact 分页 W06 被判为未做。工具能继续读，不代表平台/GUI 已能继续读。

**修复**：在现有 run-bound、digest-verified artifact 身份之上增加有界 byte offset/cursor，返回 next offset/eof；固定读同一个 sealed artifact，不允许翻页时悄悄换版本。GUI 用增量 UTF-8 解码处理跨页多字节字符。不要把一次读取上限改成无限大。

来源：[S21] [S18]。

## 缓存与上下文：下一阶段的设计取舍

### 不同问题不能叫作同一种缓存

| 层 | 核心职责 | 不能作为的证据 |
|---|---|---|
| 上下文驻留/外存 | 当前注意力和可恢复原文的放置 | 驻留不等于已发给模型；移出不等于可以删除 |
| 协议正文缓存 | 跨当轮尾部截断复用准确片段 | path@revision 命中不等于覆盖整文件 |
| 工具结果复用 | 对满足前置条件的只读结果避免重复工作 | 不能复用写操作、审批、过期验证的权限/副作用 |
| provider prompt cache | 复用服务端前缀计算 | HTTP 字节前缀相同不保证实际缓存命中；命中不等于任务正确 |

优先修 F01，而不是先扩大 4 条正文缓存上限。正确 miss 可以重读，错误 hit 会悄悄误导模型。

### 继续 CurrentStateLast，但不要冻结错误上下文

保留已有布局与 provider-default/显式 opt-in 分离。稳定规则与可复用材料放前面，当前指令、最新任务状态、完整性告警和当前工作正文放动态区域。在不改变正确性的前提下保持条目顺序、消息边界和工具 schema 序列化稳定。

缓存边界应从最终请求导出，而非预估请求。压缩或删除历史会改变可复用前缀，应该比较总成本，不能仅追求缓存率。OpenAI 当前文档也区分真实 `cached_tokens`、`cache_write_tokens` 及不同模型的缓存边界/生命周期，不能把某网关的行为当成所有兼容接口的保证。

[OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)

### 指标以完成工作为分母

保留 provider 原始 usage 的可选值及来源，区分 observed / estimated / unknown。主模型、压缩器、重试、取消都进入同一任务账目，但分别标注 call role。现有 ModelBackedCompactor 在 usage 缺失时回退估算，应明确估算身份；不能把取消/缺失 usage 填零后得出便宜结论。

建议用一批同起点、同模型 profile 的真实任务比较：合格完成数、遗漏/错误证据数、重复补读、总输入输出、缓存读写、压缩开销、重试、取消延迟和恢复成功。缓存命中率只解释成本来源，不做主验收目标。

来源：[S22] [S23] [S24]。

## 应继续保留的边界

RuntimeActor 继续唯一调度；TaskManager/TaskAnchor 留在 runtime；Core 继续处理授权、执行副作用、日志与相应权威；ContextEngine 不拥有任务授权。平台是经过授权的 RuntimeHandle 门面，GUI 是投影与明确用户操作的发起者。

已看到工作区 effect journal 使用真实 OS 独占文件锁。不能仅凭宿主的 SingleInstance 辅助 PID 文件就断言生产存在双写 split-brain：必须把更早的 Workspace::open/journal 锁也纳入调用链。

错误被判为已修复需要同任务、同可信验证 probe；同版本文件片段合并需要实际覆盖。这些已有约束应保留。不要把上述问题变成重做存储、重做权限、另起 TaskGraph、另起 GUI 或新增向量库的理由。

来源：[S07] [S09] [S25] [S26] [S27]。

## 文档调整原则

本报告新问题与下一阶段计划必须链接到固定提交和现有任务，不重开已完成 N/W 编号。`CURRENT` 记录当前默认配置、事实和仍未验收项；`NEXT_TASKS` 只保留三条活动主线、接口依赖和可交付产物；完成历史留存但不挤占 coding agent 每轮工作焦点。

先修用户可见闭环，每个新行为补最小反例回归。已有实测/验证域任务继续用原入口，本报告不再创造一套与它并行的评测工程。

## 阅读范围

下面是实际源码读取账目，不是仓库所有文件的枚举。全文只表示该文件已完整读取，不能外推为 crate 全部代码已审查。

| 文件 | 读取范围 | 限定 |
|---|---|---|
| `README.md` | 全文 | 文档，非运行证据 |
| `Cargo.toml` | 全文 | 工作区清单 |
| `docs/CURRENT.md` | 部分；响应截断 | 当前状态声明，未作为本地验收证明 |
| `docs/NEXT_TASKS.md` | 1–200；另读85–155；响应部分截断 | 仅核对已完成工作与下一步，不宣称全文读完 |
| `crates/agent-contracts/src/lib.rs` | 全文 | 接口索引 |
| `crates/agent-contracts/src/model.rs` | 1–620 | 消息、TurnFrame、布局 |
| `crates/agent-contracts/src/model_cache.rs` | 全文 | 缓存边界与摘要 |
| `crates/agent-runtime/src/lib.rs` | 全文 | 模块与公开边界 |
| `crates/agent-runtime/src/prompt.rs` | 1–1180；重点复读490–725 | 生产组装逻辑与部分测试；未覆盖全文件测试尾部 |
| `crates/agent-runtime/src/execution/body_cache.rs` | 全文 | 正文回注缓存 |
| `crates/agent-runtime/src/actor/mod.rs` | 600–920；1100–1400 | 缓存写入、部分执行/恢复状态 |
| `crates/agent-runtime/src/actor/model.rs` | 1–585；640–1620 | 主要请求准备、预算、组装、发送；585–639及后续未完整阅读 |
| `crates/agent-runtime/src/services.rs` | 1–655 | 组合缝与上下文委托；余下未完整阅读 |
| `crates/agent-runtime/src/output.rs` | 全文 | 最终输出限幅 |
| `crates/agent-runtime/src/task.rs` | 1–330 | 任务锚点、完成策略；非完整任务管理器审查 |
| `crates/agent-runtime/src/work.rs` | 全文 | 提交收据和进程内去重边界 |
| `crates/agent-runtime/src/platform.rs` | 1–300 | 平台路由与操作查询；余下未完整阅读 |
| `crates/agent-runtime/src/platform/work.rs` | 1–310；600–870 | 授权、任务详情、变化、artifact、context读取；其余未完整阅读 |
| `crates/context-simple/src/lib.rs` | 全文 | 模块索引 |
| `crates/context-simple/src/engine.rs` | 1–300；850–1110 | 配置、部分状态、真实消息/工具摄入；其余未完整阅读 |
| `crates/context-simple/src/materializer.rs` | 1–970 | 主要选择、范围覆盖和前景计划；其余未完整阅读 |
| `crates/context-simple/src/gc/reachability.rs` | 全文 | 决策/错误/正文语义终结 |
| `crates/context-simple/src/index/entity.rs` | 1–300 | 实体抽取、精确匹配、最新文件根；测试尾部未完整阅读 |
| `crates/context-baselines/src/lib.rs` | 1–300 | 入口及部分测试 |
| `crates/context-baselines/src/rolling.rs` | 全文 | Rolling维护、回滚、materialize |
| `crates/context-baselines/src/shared.rs` | 全文 | 基线摄入及材料化映射 |
| `crates/tool-runtime/src/lib.rs` | 全文 | 工具模块索引 |
| `crates/tool-runtime/src/tools/mod.rs` | 1–260 | 有界目录遍历 |
| `crates/tool-runtime/src/tools/fs.rs` | 1–600 | 文件读取/列表的主要生产逻辑及部分测试 |
| `crates/tool-runtime/src/tools/search.rs` | 1–580 | 搜索主要生产逻辑与部分测试 |
| `crates/agent-core/src/lib.rs` | 全文 | 只完成模块与权责边界检查，非Core全实现审查 |
| `crates/agent-workspace/src/lib.rs` | 1–280；340–675 | artifact、工作区打开、部分修改事务；非完整文件审查 |
| `crates/agent-workspace/src/broker.rs` | 1–340 | 输出broker全部生产逻辑及部分测试 |
| `crates/agent-workspace/src/journal.rs` | 1–270 | 独占锁、WAL结构和部分写入；恢复算法未完整阅读 |
| `crates/provider-openai/src/prompt_cache.rs` | 全文 | provider缓存模式 |
| `crates/provider-openai/src/diagnostics.rs` | 全文 | 请求身份和usage诊断 |
| `crates/agent-compose/src/lib.rs` | 1–330 | 上下文引擎与provider组合入口；其余未完整阅读 |
| `crates/agent-compose/src/compactor.rs` | 全文 | 有界压缩器及相关回归 |
| `crates/agent-host/src/lib.rs` | 1–340 | 宿主边界、帧、锁辅助与部分恢复；余下未完整阅读 |
| `crates/agent-host/src/main.rs` | 1–340 | 生产启动与端点选择；余下未完整阅读 |
| `clients/dotnet/Agent.Client/Transports.cs` | 全文 | 真实连接运输选择 |
| `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs` | 全文，分段读取至文件结尾 | 生产GUI ViewModel；不等于所有GUI文件 |

尚未完整逐文件审查：process/capability-process、完整协议实现、storage/replay/eval/TUI、provider 主传输/Responses/SSE/retry、Core 执行全部实现、runtime 恢复/维护/工具全链路、context-simple 全 GC/store/检索、其他 .NET 客户端和 GUI/XAML、全部测试及打包脚本。因此不对这些区域出具“没有问题”的保证。

## 固定源码索引

[S01]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/tool-runtime/src/tools/fs.rs#L1-L600
[S02]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/execution/body_cache.rs
[S03]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/actor/mod.rs#L600-L920
[S04]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/prompt.rs#L490-L725
[S05]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-workspace/src/broker.rs
[S06]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-simple/src/materializer.rs#L640-L970
[S07]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-simple/src/gc/reachability.rs
[S08]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-simple/src/index/entity.rs#L1-L70
[S09]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-simple/src/engine.rs#L850-L1110
[S10]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-host/src/main.rs#L1-L340
[S11]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-compose/src/lib.rs#L1-L160
[S12]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-baselines/src/rolling.rs
[S13]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/context-baselines/src/shared.rs
[S14]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/actor/model.rs#L640-L1620
[S15]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/tool-runtime/src/tools/search.rs#L1-L580
[S16]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/tool-runtime/src/tools/fs.rs#L1-L300
[S17]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-contracts/src/model.rs#L1-L300
[S18]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs
[S19]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/clients/dotnet/Agent.Client/Transports.cs
[S20]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/work.rs
[S21]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/platform/work.rs#L600-L870
[S22]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/provider-openai/src/prompt_cache.rs
[S23]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/provider-openai/src/diagnostics.rs
[S24]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-compose/src/compactor.rs
[S25]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-workspace/src/journal.rs#L1-L270
[S26]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/services.rs#L1-L655
[S27]: https://github.com/Ye-Luo-0912/context-agent-prototype/blob/685b6bbb29275bc8ec73ce6625a94567a8b8d23d/crates/agent-runtime/src/platform.rs#L1-L300
