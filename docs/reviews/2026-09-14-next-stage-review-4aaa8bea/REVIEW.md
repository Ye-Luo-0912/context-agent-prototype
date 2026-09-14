# 后端长流程下一阶段：源码续审与可维护性实施计划

审查日期：2026-09-14（UTC）。
固定源码：`4aaa8bea89336e2ec0fd21c76d04967814b24020`。
仓库：`Ye-Luo-0912/context-agent-prototype`。

## 结论

下一阶段应从分散的审查修补，转向**可持续使用的后端开发流程**：同一任务完成计划、检索、修改、验证、中途纠正、取消、冷恢复与交付；Context/GC 的热资源与单次工作量有边界；供应商 KV 缓存的收益按完整任务计算。

可维护性属于每个功能切片的验收条件，不另开全仓重写阶段。GUI 维持必要兼容和正确性修复，不扩展功能。

保留 RuntimeActor 唯一编排者、Core 的权限/效果/提交权威、TaskAnchor/ExecutionState、已有 checkpoint 和公开 host/SDK。以下模块名或类型名如 `FinalizedRound` 只表示建议的责任边界，不表示仓库已经存在相应类型，也不要求新增 crate。

## 证据与范围

- 已核对根目录、workspace 20 个 crate 清单、相关源码子树，以及下文列出的当前文件正文范围。
- 固定源码 CI：run `34788369834`，最终 success，`run_attempt=2`。这不是首次运行全绿，也不是本次审查者执行的测试。
- 已读取阶段收尾回执：它将不同旅程环节映射到已经执行的不同回归。该证据有价值，但不等于一条同任务、同工作区、跨进程重启的完整执行轨迹。
- 本次 GitHub 容器访问出现 DNS 解析失败，未取得完整本地 checkout；容器未发现 Cargo/.NET。未执行仓库 Rust/.NET 测试、文档检查、运行性能测量或付费模型实验。
- **本次不是全部生产文件、测试、SDK 和脚本的逐行审查。** 目录枚举、文件大小、搜索命中均不计作正文通读。没有审阅到的路径不视为通过。
- 当前发现主要为 P2 正确性、效率和维护风险。未确认新的 P0；不为了形成任务队列提高严重等级。

## 已有成果：保留，不原样重开

1. 文档入口已分离职责，根 AGENTS 不再将旧 GUI 主线作为当前目标；文档检查明确只验证机械结构。
2. 冷元数据读取的 pending owner 保全、有界卡片读取、校验及 hydration 完整性到删除许可的传播已有实现。
3. process.session 已区分普通退出和信号退出，已有独立会话锁及批次限制；一次性进程的 grace 修复已有提交。
4. MCP tools/list 已支持分页，不再属于“只读第一页尚未实现”。
5. Runtime 已填写缓存路由键，Responses mapper 已有多断点映射，已有真实 Compose→Runtime→Provider→本地 HTTP 的接线测试。
6. 正式 host 默认仍为 Rolling；不能因建议推广 Dynamic 就把默认切换视作已发生。

## 当前发现

### R1 / P2：最终装箱在分区内选择可选项，却没有全局优先保护必需正文

定位：`crates/agent-runtime/src/actor/model.rs`：`is_required_context_body`、`largest_final_pack_drop_index`、`continue_model_operation_after_materialize`。

当前顺序为：裁 `materialized.items` → 裁 `foreground` → 裁可选工具 schema。每个正文列表内部先找可选项，找不到就退回必需项。因此 selected 中只剩必需正文时，即使 foreground 还有可选大正文，也会先裁掉必需正文。

这是控制流确认的条件性错误：存在可以保留必需正文的合法装箱，当前算法仍可能产生 `BudgetExcluded` 必需缺口。不是所有请求都会触发，也不是已执行的 Rust 复现。

建议：在最终装箱层统一构建带来源分区、身份、必需等级、预计大小的候选视图。优先移除可选内容；各类可选正文和 schema 的取舍可由现有策略决定，但不能在仍有足以释放空间的可选候选时提前牺牲必需正文。协议完整性、硬窗口、必需 schema 仍是约束。

必要反例：selected=必需小正文，foreground=可选大正文；总输入略超预算。完成装箱后必需正文保留、无对应 required miss。再验证全部必需内容确实超限时仍诚实报告，不伪造覆盖。

### R2 / P2：最后一次重组请求后，计数仍可能来自旧请求

定位：同一文件，撤销 settlement projection 的分支及其后 `estimated_input_tokens` / `packing_input_tokens` 赋值。

该分支重新构建 `input` 和可能的 `packing_input`，但没有同步重新计算 `input_total` / `packing_total`。后续预算判断和 Ready report 继续使用旧值。

影响：特定投影配置下的计数与实际请求不一致，边界上可能发生多余裁剪或拒绝。默认未启用相关投影时并非每轮触发。本次没有证明该分支导致任意超窗发送。

建议：R1/R2 合成一个功能切片。所有装箱、投影修订结束后，只有一次不可变结果发布：最终消息、工具、覆盖报告、计数、缓存计划来自同一最终输入。需要增量计数时，先证明与完整重算等价；不能继续依赖维护者手动记得在各分支更新几个变量。

### R3 / P2：无删除或延期的 Storage GC 仍使整个外置目录失效

定位：`context-simple/src/store.rs::commit_storage_gc` 与 `index/external.rs::{take_all,replace_all}`。

即使没有删除结果，commit 仍对全部 external entries 做 take/replace。两者都会清空 `card_hashes` 并触发目录重建。后续 checkpoint 因此可能重新序列化未变化的元数据；已有同名卡片可避免实际磁盘重写，所以不能把它说成“每次重写全部卡片”。

建议：删除决策为零时，保留结构与派生缓存，仅更新必要观测。部分删除时按真实成功/NotFound 的 id 更新，保留其他条目的有效 card hash。不要用全量 `&mut` 或 take/replace 表示任意局部修改。

回归：已记录卡片的 N 个条目 → 零候选 GC / 不完整根延期 / 部分删除。前两者仍保留全部原 card claims；部分删除仅对应 id 消失，后续 capture 不重新序列化所有幸存者。

关联的规模改进：`run_storage_io` 限制并发，却在派发循环完成后才汇集 JoinSet 结果；并发任务数有界不代表累积的完成结果和整个候选计划也有界。应与 T4 的单批总工作量设计一起处理，不单独制造一个新 GC 框架。

### R4 / P2：MCP 分页的总量与期限只在下一次循环入口检查

定位：`agent-capability-process/src/mcp.rs::list_tools_with_cancel`。

当前上限为 16 页、512 工具、30 秒总期限。工具数量检查在读取页前，最后一页追加后可以直接 `Ok(tools)`；总期限没有约束正在进行的页请求，最后一页也可能在总期限后成功返回。cursor 检查只保留有限的前序信息，不是完整的已见 cursor 集。

范围限定：Core 准入另有 `MAX_TOOLS_PER_CAPABILITY=32`，所以不能把发现函数的问题说成无限制安装或权限绕过。每页帧及请求也有边界。

建议：接纳每项前/最终返回前检查总工具数与累计字节；把剩余总期限传入实际交换边界；明确取消或超时后的 poison/kill/reap 结算。游标循环用有界 seen set 检查。较大的发现上限与较小准入上限可以是有意分层，但必须明确对应用户能否筛选工具，而不是收完后才无说明地拒绝。

回归：单页 513 工具无 nextCursor、最后一页超总期限、用不同工具名或空页构造 cursor 循环。不要让“重复工具名拒绝”替代对“重复 cursor 拒绝”的验证。

### R5 / P2：清理的 Unconfirmed 结果被适配器抹掉

定位：`agent-process/src/supervisor.rs::{ProcessReapOutcome,reap}`；`agent-capability-process/src/mcp.rs::reap`。

Supervisor 明确返回 Confirmed/Unconfirmed；MCP helper 忽略结果并将 supervisor 清空。Drop 仍有后备 kill，因此本次没有证明子进程必然存活或一定发生泄漏。但上层失去了“清理尚未确认”的类型化事实。

建议：关键结算结果使用 `#[must_use]`，调用方明确选择传播、保留待结算责任或记录不确定性。不要把所有 helper 改为 Result<()> 后继续 `.ok()`；真正需要保留的是领域结果。

回归采用可注入的 supervision seam 模拟 Unconfirmed，不依赖创建不可杀进程或等待长时间 OS 异常。

### R6 / P2：缓存路由键的字符串拼接不是无歧义的不透明编码

定位：`agent-contracts/src/model_cache.rs::PromptCacheRouting::key_for`。

五个字符串通过 `|` 拼接，没有转义或长度前缀；整个结果过长时截断。配置为 `(a|b,c,d,t,main)` 与 `(a,b|c,d,t,main)` 可产生相同结果；短字段保留原文，正式 host 还传入工作区路径，所以不应将该字符串称为不透明摘要。

这属于路由身份/表示问题，**不是供应商账户隔离被绕过，也不意味着不同 prompt 会错误共享生成结果**。

建议：对带版本的结构化 tuple 做规范序列化，使用现有 SHA-256/ContentDigest 设施生成固定长度摘要；不截断结构本身。主调用与维护调用的身份、端点/profile 的关系在同一组合点定义。不要使用每轮随机 id 或完整动态请求摘要作为稳定路由键。

回归：分隔符、Unicode、超长字段、不同 lane/任务、同任务冷恢复。更改键编码会造成一次缓存路由迁移，记录这一事实即可，不需要兼容两套长期键逻辑。

## KV 适配的另一个待核验边界：接线测试不是供应商 schema 校验

当前 mapper 的 declared-breakpoint 路径在消息 content 仍是字符串时，把 `prompt_cache_breakpoint` 写在 input item 的同级。官方文档展示的是支持的 content block 上的断点。现有本地 HTTP 捕获服务器收集 JSON 后返回预设 SSE，并不执行供应商请求 schema 校验。

因此要区分三件事：客户端确实发出了字段、特定端点确实接受该字段位置、后续请求确实获得缓存收益。不能从第一个推导后二者。本轮未访问付费端点，不能把这种编码差异直接记成已经实测的 400。

下一片以具体支持端点为单位核对请求形状：官方 Responses 走其文档定义的 content-block 形式；确实存在的网关扩展才单独声明。对应独立的 schema/fixture 断言，不让测试只复制当前 mapper 的输出作为“正确答案”。

同时，旧单边界有 prefix+tools 摘要验证，新裸 `cache_breakpoints` 列表是另一条路径；应收敛为一个经过最终输入验证的 CachePlan，避免新增断点表面绕开旧 hint 的失效规则。此处首先影响断点范围与写入策略，不代表供应商会复用错误内容。

## 可维护性：重构哪些边界，哪些不要动

### 不是按行数拆文件

已见热点文件大小如下，均为 Git blob 字节数，包含注释和可能的内联测试，不是生产 LOC 或复杂度评分：

| 文件 | 字节 |
|---|---:|
| agent-runtime/src/task.rs | 193955 |
| agent-runtime/src/prompt.rs | 167161 |
| context-simple/src/store.rs | 151065 |
| context-simple/src/engine.rs | 148763 |

优先顺序不是从最大文件开始，而是从最容易出现语义漂移的规则开始。

### 建议收敛的四个责任边界

**最终请求构建**：Actor 负责调度、验证身份和发布；独立的纯装箱逻辑负责候选取舍与最终输入。原 `ModelRoundPlan` 和 `ModelInput` 可以继续使用。最终计数、coverage、schema snapshot、cache plan 不再各自重建一份“近似相同”结果。

**外置目录与所有权操作**：生命周期迁移、索引更新、card claim 作废与 catalog dirty 必须通过具名操作。已有 ExternalMap 是基础，不必重建；减少依赖“get_mut 调用者不得修改某字段、必须另行标 dirty”的约定。局部改变只失效相关派生数据。

**进程执行与结果结算**：复用已有 agent-process 和 stream capture，提炼期限、排空、退出观察与 cleanup outcome 的公共机制；shell 的解释器语义、process.run 的 argv 语义、session 的跨调用生命周期、verify 的证明权威保留在各自策略层。不要强行压成一个巨大的通用 runner 配置对象。

**产品运行配置**：RuntimeServices 构造器中已有重复初始化和多个实验 bool。将稳定产品配置与实验投影参数分组，汇合到同一构造路径；入口只解析差异，不重复设置每一个底层字段。保留冻结实验语义。Rolling/Dynamic/service 的维护预算作用不同，必须在 effective config 中诚实区分“不支持/未接入”与“默认”。

### 测试也属于维护性

- 先保留旧行为的特征化回归，再做等价抽取；行为修改与机械移动尽量分开提交。
- 关键测试断言外部结果/状态/字节，不依赖源码里出现某一行字符串。
- 反例必须排除其他拒绝分支抢先触发，否则只是“测试红了”，不是证明了目标规则。
- 故障注入放在 test-only seam 或隔离工作树。仓库已有 grace 修复曾被遗留 RED_CHECK 短路的记录，不能在共享工作树改生产代码做红检查后依靠记忆恢复。
- 本地捕获 client 已经显式 `no_proxy()` 时，不需要额外修改全局 NO_PROXY；避免测试间隐式全局状态依赖。
- 不以拆文件数量、增加测试数量、提高覆盖率百分比或消灭所有 lint 作为本阶段产物。评价改变一条规则所需修改位置是否减少、是否出现重复权威、关键路径是否仍可解释。

## 下一大阶段的八个任务切片

以下 T 编号仅在本报告内使用。写回现有 NEXT_TASKS 时复用当前任务体系，不并行维护第二套待办。

### T1 — 统一最终装箱与发布（B/C 共享，单一集成人）

**用户结果**：预算不足时优先保留真正必要的证据；模型实际输入与观测、覆盖和缓存边界一致。

修改入口：actor/model.rs、prompt.rs、model/model_cache 契约及现有装箱测试。先修 R1/R2，再抽取稳定的纯决策函数。统一候选优先级，最终结果一次生成计数与覆盖。

验收：R1 两分区反例；必需 schema/协议配对不破坏；final projection 改变后计数完整重算相等；非法 cache plan 不导致错误断点；无 over-budget 伪发送。

停止：不改变 Context 打分/GC 算法、不重做全部 prompt 文案、不新增事件数据库。

### T2 — 外置目录的增量维护（B）

**用户结果**：无变化的维护不反复付序列化和目录重建开销，冷恢复保证保持。

修改入口：store.rs、index/external.rs、checkpoint/hydration 相关现有测试。修 R3；把针对单项的结构变更收敛到具名目录 API。

验收：零删除/延期时 cache claims 不变；部分删除保持其他 entries/indexes/cards；现有恢复根和 pending owner 反例全部保持。

停止：暂不引入新数据库。真正冷驻留在 T4 处理，不把 checkpoint 分片冒充内存分页。

### T3 — 有界进程与能力发现（A）

**用户结果**：长工具可以持续轮询、停止，退出/输出/清理事实清楚；MCP 发现不能超出声明预算悄悄成功。

修改入口：mcp.rs、supervisor.rs、session/process/shell 与现有 stream capture。修 R4/R5；只抽取已经重复且职责一致的机制。

验收：末页总量与总期限；游标循环独立反例；Unconfirmed 传播；信号/非零退出/未排空不同；取消和慢 I/O 的结果诚实。

停止：不新增进程调度器，不扩展 MCP 协议范围来替代修复已声明范围。

### T4 — 真的有界冷目录、搜索与维护工作（B）

**用户结果**：历史增加后，常用任务不因首次搜索或 GC 全量重水化历史而大幅停顿；未读区域始终可检索且不被误删。

在现有 store/index/ContextEngine 内实现持久目录或分页查询视图、有上限热元数据缓存、受保护 roots/pinned/dirty 条目的明确规则。选择存储实现前先记录当前元数据规模和查询工作量，不先决定必须向量库或新服务。

每操作分别定义：items/bytes/I/O 量/绝对时间预算；超出后返回可继续状态或明确不完整，不把总工作量推给一个隐藏的 `hydrate_all` 循环。搜索的“排名 Top-K”和“因读取失败/预算而覆盖不完整”不同，应有类型化区别。

验收：以设定热预算构造明显大于预算的冷集合；恢复、按 id fetch、搜索、GC 往返后热资源不随全历史永久增长；未读依赖闭包时延期删除；候选排序/结果稳定性有版本依据。

停止：第一版只证明声明规模下的边界，不宣称无限历史、任意故障都常数时延。

### T5 — 一份有效产品配置到所有入口（A/C 共享）

**用户结果**：host、TUI/headless、SDK 和明确支持的 service 路径对当前策略、预算、权限、恢复和完成方式没有隐含分歧。

复用 compose/services，把产品与实验设置分组，共用默认构造和校验。正式 host 默认 Rolling 先保持；为 Dynamic 明确可启用 profile、回归范围和实际限制。正数维护预算/backoff 的适用引擎要说明，不能仅因打印了相同字段就称所有引擎同等执行。

验收：读取 effective config 与执行时实际选择一致；不支持的组合拒绝或显式报告；未知修改不自动重放；普通 final 与持久完成区别保持。

停止：不更换 GUI 技术、不添加多工作区总调度、不任意改变已有公开 wire。

### T6 — 供应商 KV 的稳定布局（C，依赖 T1）

**用户结果**：连续请求复用有效、预计重复使用的前缀，减少无必要的缓存写入，而不延迟新事实或新指令生效。

先修 R6，核对具体端点的断点编码。沿当前 EvidenceSplit/ModelInput 构建受约束的有效证据基座和动态尾部：稳定规则、较稳定证据、近期必要协议、最新控制状态分开；按确定规则排序；频繁编辑正文不强留稳定区。

“epoch”须代表实际的跨轮有效快照/布局规则，不只是把每轮全新 selected block 命名为 epoch。派生布局的字节也计入内存预算，不持有第二份无界历史；权限撤销、来源版本改变、用户移除要求、必需证据缺口和硬窗口必须即时生效。

先评估稳定证据前缀；协议尾缓存作为单独可选择的扩展，保持完整角色和 tool call/result 配对。不同供应商是否支持、TTL/写入计价/断点数量由具体能力契约决定，不把任意内容 hash 当作任意块 KV 可拼接。

验收：相同有效基座+不同焦点/缺失/恢复信息保留相同基座；文件修改、工具撤销、任务切换确实失效；A→B→A 重组不冒称原有后缀自动命中；最终 wire 的断点位置合法。上述可用无付费模型的请求序列完成；缓存收益必须由 T8 证明。

停止：不追求所有供应商一次覆盖，不为命中率保留过时证据。

### T7 — 同一任务的完整后端开发流程（A 主持，三线共同）

**用户结果**：一个真实跨模块代码任务可在同一 TaskId、同一工作区及恢复 lineage 内完成，而非一组无关测试相加。

复用现有 host/headless 和测试 harness，以实际读写、真实进程、checkpoint 文件、受控模型决策驱动：

`提交 → 计划/验收约束 → 检索读取 → 修改 → 协议尾回收与证据找回 → 验证 → 用户纠正 → 中断/保存 → 退出进程 → 新进程冷恢复同一任务 → 重新核对外部改动 → 完成剩余修改与验证 → 提交结果/等待操作员关闭`。

同任务遇到慢维护或临时冷页故障时不得丢指令/owner；已发生效果不重放。根据实际引擎能力将 service 模式作为明确变体，不强迫一个运行同时冒充所有引擎。

验收：最终代码/测试产物、任务身份与输入 lineage、未决效果、清理状态、使用的有效配置、证据和费用完整性均可核对。允许复用已有断言，但不能把多个不同任务的成功行拼成一次执行成功。

停止：一条代表性综合流程及关键故障变体完成后继续功能开发，不把它变成无限新增门禁。

### T8 — 真实模型质量与任务全成本对照（C，条件任务）

客户端链路不因缺凭据被搁置；真实质量/成本声明必须等待实际执行。有授权预算和凭据时固定起点、任务、产物 oracle、模型/端点/profile、重复策略及缓存冷热条件。

同时记录主调用与维护调用的每次真实 attempt；同 attempt 累计 usage 快照不重复计费，不同 attempt 不遗漏；普通输入/缓存读/缓存写/输出按供应商口径正规化，缺测独立记录。费用还包括适用的存储/工具成本。

比较的是验收质量保持时的每完成任务成本、轮次、重复证据读取、无效验证、维护开销、缓存重写和恢复行为。不是单看 cached ratio 或输入 token 数。

无预算/凭据保持 NOT_RUN。接线已完成、API 接受、缓存命中、质量保持、费用下降分别给结论。

## 并行安排

先并行 T1/T2/T3 的有限修复与责任边界抽取。T1 公共契约由一个集成人维护。

随后 T4 冷目录与 T6 KV 布局并行，T5 组合点只接入已明确语义；最后 T7 综合流程收尾。T8 按授权条件执行，不为等付费实验反复重跑无关测试。

每片先写用户动作，补目标反例，再做必要实现和小范围等价抽取。功能修改和大范围文件移动不混在同一提交。达到停止条件即结束该片。

## 可执行检查建议（本轮未执行）

在实际工作树执行前，先查看现有修改，不能覆盖其他协作者的未提交代码。

```sh
git status --short
git rev-parse HEAD
git diff --stat

# 只选择本片涉及的 crate；不要为每个小提交重复所有命令。
cargo test -p agent-runtime --lib
cargo test -p context-simple --lib
cargo test -p agent-capability-process
cargo test -p agent-process
cargo test -p provider-openai
cargo test -p agent-compose --test cache_wire_flow
cargo test -p tool-runtime

# 本片/集成适用时执行，继续使用仓库已钉住的工具链。
cargo fmt --all -- --check
cargo clippy -p agent-runtime -p context-simple -p provider-openai --all-targets -- -D warnings
python scripts/doc_consistency.py
```

上述不是一个要求每片全部执行的新总门禁。新增测试使用过滤名时先确认能列出测试，不能用“0 tests passed”关闭任务。Windows/Linux 专属行为由对应环境或原 CI 矩阵验证，不能在另一个平台标为已执行。

## 文档更新方式

这份文件是固定基线的一次审查与阶段提案，不应全文复制到 CURRENT/NEXT_TASKS。把被采纳的 T 切片写入唯一当前队列；CURRENT 只保留核对版本、有效 profile、能力与限制。已关闭旧项留原回执链接即可。新的 CI 成功只绑定相应 SHA/attempt，不倒推到其他代码版本。

## 本轮正文读取范围

以下为当前 SHA 实际读取的正文范围；请求的末行超出 EOF 时以实际返回为准。未列出的文件不计入正文覆盖。

| 文件 | 本轮范围 |
|---|---|
| AGENTS.md | 全文 |
| Cargo.toml | 全文 |
| docs/CURRENT.md | 全文 |
| docs/NEXT_TASKS.md | 全文返回范围 |
| docs/reviews/2026-09-14-backend-review-6eda2474/STAGE_CLOSING_JOURNEY_RECEIPT.md | 全文 |
| scripts/doc_consistency.py | 全文 |
| crates/agent-runtime/src/actor/model.rs | 1–365、520–820、920–1665 |
| crates/agent-runtime/src/prompt.rs | 305–590 |
| crates/agent-runtime/src/services.rs | 1–300 |
| crates/agent-runtime/src/task.rs | 1–285 |
| crates/context-simple/src/engine.rs | 2290–2520 |
| crates/context-simple/src/store.rs | 1250–1870 |
| crates/context-simple/src/index/external.rs | 1–340 |
| crates/agent-capability-process/src/mcp.rs | 135–345 |
| crates/agent-process/src/supervisor.rs | 全文返回范围（1–360 请求） |
| crates/agent-core/src/capability_admission.rs | 1–225 |
| crates/agent-compose/src/lib.rs | 80–180 |
| crates/agent-compose/tests/cache_wire_flow.rs | 1–190 |
| crates/agent-contracts/src/model_cache.rs | 全文返回范围（1–310 请求） |
| crates/provider-openai/src/prompt_cache.rs | 全文 |
| crates/provider-openai/src/lib.rs | 770–1148 |
| crates/agent-host/src/main.rs | 100–300 |
| crates/tool-runtime/src/tools/session.rs | 1–300 |
| crates/agent-conformance/src/checks.rs | 1–250 |
| clients/dotnet/Agent.Client/ResumableSession.cs | 1–280 |

额外取得根/20 crate/部分 src 树的文件元数据、代码搜索结果和 CI 元数据。这些只用于导航或限定结论。大型递归树可见响应存在截断，未据此声称全仓文件逐项审阅完毕。上轮已读但本轮未重新核对的内容不当作新的全量覆盖。

## 主要固定来源

- [固定版本](https://github.com/Ye-Luo-0912/context-agent-prototype/tree/4aaa8bea89336e2ec0fd21c76d04967814b24020)
- [CI run 34788369834](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34788369834)
- [最终装箱与发送](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/agent-runtime/src/actor/model.rs)
- [PromptAssembler](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/agent-runtime/src/prompt.rs)
- [Store](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/context-simple/src/store.rs)
- [ExternalMap](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/context-simple/src/index/external.rs)
- [MCP](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/agent-capability-process/src/mcp.rs)
- [Supervisor](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/agent-process/src/supervisor.rs)
- [缓存共同契约](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/agent-contracts/src/model_cache.rs)
- [Provider wire](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4aaa8bea89336e2ec0fd21c76d04967814b24020/crates/provider-openai/src/lib.rs)
- [OpenAI 官方 Prompt Caching 文档，核对日 2026-09-14](https://developers.openai.com/api/docs/guides/prompt-caching)

外部文档只用于缓存前缀与支持请求形状等兼容性核对；它不是该仓库已经降本或端点实测成功的证据。本文件没有更改仓库，也没有替用户启动付费调用。
