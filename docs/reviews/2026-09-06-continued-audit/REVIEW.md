# 仓库续审：产品主线与 Context / GC / Search 下一版

核对日期：2026-09-06（日本时间）。固定源码：`12c86283b8d5991e9f17a07f14871dcf39d65066`。

**结论：继续现有单 RuntimeActor 架构；沿 D0、F1–F6 交付产品；核心资产从“生命周期机制齐备”进入“证据可见性正确、检索有效、维护有界”的版本迭代。不将 Chronicle、TaskGraph、全新调度平台或新一轮大型评测变成这些功能的前置条件。**

## 1. 审查对象、证据与边界

本轮开始与结束两次读取 `main`，均为上述 SHA，与上轮任务文档包的基线相同。GitHub Actions run `33986702977` 的实际状态为 completed / success。本轮查询 `docs/NEXT_TASKS.md` 返回 404：上轮提供的替换稿没有出现在这个 main 快照中；这不说明用户其他分支或本地工作树没有改动。[S1][S2]

本轮完成的是：19 个 crate 的职责和端到端链路梳理，结合前轮同一源码的阅读，进一步深入 context scorer、catalog、倒排内核、冷读、GC、影子 Frame、进程执行、TUI 状态恢复和离线汇总。**没有完成所有仓库文件逐行审查，也没有本地 Rust 全仓构建或测试。**根递归树的工具返回被截断；完整源码 ZIP 获取未成功；容器无 cargo/rustc。不能把目录枚举、远端 CI 成功、附件旧报告的执行声明算成本轮全量跑测。

`READ_COVERAGE.csv` 只列本轮新增/重读的正文路径、请求范围和截断情况，不是全仓文件清单，也不是逐文件全部通过的证明。其余模块的架构判断沿用前轮源码阅读和本轮目录核对，不代表本轮重新逐行认证。

唯一实际执行的运行实验是一个 Linux 子进程前提探针：子进程先关闭 stdout/stderr，再继续休眠。两个管道确实先返回 EOF，而子进程仍活着。见 `pipe_eof_probe.json`。探针已杀死并回收自身进程组。**这验证的是触发条件，不是实际运行仓库 Rust 代码后复现全部故障。**

附件《审查仓库完整性.txt》含较早的可靠性与 Chronicle→TaskGraph 阶梯方案。本轮将它作为历史意见，既不继承其“已实际跑完测试”的声明，也不重新施加它的固定阶段顺序。当前已有 Run Summary、状态投影等实现，且用户已明确要求减少过度工程化。

## 2. 架构梳理：哪些保留，哪些补接线

下表覆盖工作区的全部 19 个 crate，但覆盖含义是职责地图，不是每个文件均深审。

| 模块组 | 职责 | 下一步边界 |
|---|---|---|
| agent-contracts、agent-platform-protocol | 进程内契约、工具/上下文/事件类型、独立有界协议 DTO | 在原类型上修证据片段与输出覆盖契约，不建立平行协议 |
| agent-core、agent-workspace、agent-storage | 权限、操作/副作用身份、受限文件访问、journal 与持久化屏障 | 保留；不为功能接线重写 authority/WAL |
| agent-runtime | 唯一 Task/Turn 调度、Prompt、工具表面、完成与恢复协调 | 接好 continuation、计划查询、有限执行段、异步证明 |
| context-simple | 历史工作集、生命周期、驻留、catalog、外置和召回 | 先修真实性，再优化候选与维护成本 |
| context-baselines、context-contextcore、agent-context-service | 对照策略、ContextEngine 进程适配与服务端 | 保留对照与兼容；默认产品不等待实验服务的全部扩展 |
| tool-runtime、agent-process、agent-capability-process | 工具、共享进程生命周期、能力/MCP 适配 | 修实际进程等待/清理路径与检索边界，不另造执行器 |
| provider-openai | 模型请求、流协议、重试和用量 | 继续单一传输职责，不迁入任务权威 |
| agent-compose、agent-tui | 可信组合与产品交互 | 优先让已有能力正常可用，配置与实际行为一致 |
| agent-replay、agent-eval、agent-conformance | 重放、诊断、评测、契约检查 | 修现有观测错误，复用检查；不以新增门禁数量作为交付 |

### 2.1 主执行链

```text
用户输入 / continuation
  → TaskAnchor + ExecutionState（Runtime 权威）
  → 既有安全点：重验、有限维护、工具需求投影
  → ContextEngine::materialize（历史工作集预览）
  → PromptAssembler + 当前 TurnFrame + schemas
  → 最终预算检查与请求快照
  → 模型
  → 可信工具事实 / Core 权限与 effect 结算
  → 任务进度、验证、消费确认
  → 下一轮，或安全停止 / 完成 / 检查点
```

这不是一条统一的“聊天记录压缩”链。当前 TurnFrame 的工具协议、长期 Context 工作集、任务权威字段、工具 schema 是不同平面。不能只调历史 GC，就假定当轮长工具循环的所有成本都会改善。

### 2.2 记忆闭环

```text
保存/外置 → 索引定位 → 搜索 → fetch（只读）
                       → 必要时 admit（生命周期迁移）
                       → 物化和实际渲染 → 消费 ACK
                       → 注意力/驻留维护
```

搜索候选不等于选入；选入不等于正文实际展示；正文展示不等于模型使用正确；fetch 不等于 admit。用一条“访问次数”替代这些区别，会把检索和 GC 调成相互强化的噪声循环。

## 3. 新发现与已知问题的分流

| 事项 | 本轮判断 | 归属，不另开阶段 |
|---|---|---|
| 输出管道 EOF 后单独 wait，停止轮询 timeout/cancel | 静态控制流确认；OS 触发前提已实测 | F3 |
| TUI resync 选择最旧文件、混入其他 run、缺重放水位 | 静态确认；影响显示，不是 Core 权威变更 | F4；相关错误反馈可随 F1 |
| Run Summary 读取不存在的 required_misses.total | 静态契约对照确认 | F4 |
| Shadow Frame 将 cap 省略计为 duplicate，部分门禁只检查标签 | 静态确认；默认关闭的观测路径 | 研究附带修复，不挡 F1–F4 |
| 检索候选分数未进入最终排序 | 现有设计，不宣称违反契约；优化接入点 | 核心下一版 |
| 广义语义查询需补读大量 Stored 正文 | 现有有界设计；需测真实成本 | 核心下一版 |
| release ACK 实际更新在 debug_assert 内 | 前轮发现，本轮仍存在 | F1 |
| 非 Pinned PromptRequired 可能重复选入 | 前轮发现，本轮仍存在 | F1 |
| 同文件版本被当成同一正文覆盖 | 前轮发现，本轮渲染路径仍相同 | F5 |
| 成功观察/实体关联被用作错误终结；lease 跨层语义问题 | 前轮已定位，非本轮全链路重新复现 | F5 定向核对/修复 |

“必须修这条使用路径的真实错误”与“所有模块必须先清零才开发功能”不是一回事。前者保留，后者撤销。

## 4. 关键发现的原因、影响与最小修复

### 4.1 process.run / shell.exec：输出结束并不代表进程结束

`tools/process.rs` 的正常循环同时等待 cancellation、deadline、child.wait 与 line_rx.recv；但 recv 返回 None 时，如果 child 尚未退出，会在分支内直接 `child.wait().await`。`shell.rs` 有同类代码。此后原 select 中的取消与超时不再被轮询。[S3][S4][R1]

触发路径：程序合法地关闭两个输出流，随后继续工作、休眠或卡住。不能把管道 EOF 当成 process terminal。Linux 探针实测两个管道在约 3.817 ms 内 EOF，而 child.poll() 仍是 None；探针使用的 30 秒休眠已立即清理，没有留下后台进程。

修复只需要保留单一等待循环：标记 `output_closed = true`，禁用之后的 recv 分支，继续轮询 wait/cancel/deadline。不能对永久就绪的关闭 receiver 直接无限 continue，否则可能形成忙循环。超时/取消后依旧执行既有 kill 与 reap。

最小相关回归：进程先关闭两个输出但不退出，验证有限 timeout；另一次验证 cancel。测试自身设置外层 watchdog，避免旧代码挂死测试进程。不要为此建立新的通用进程调度框架。

### 4.2 proof deferral：已有实现应接入，但“abort”不能代替清理确认

`HostProofVerifier` 给 runner 新建 CancellationToken，未把 Actor 的取消令牌传下去；TUI 又将 `defer_proof_refresh` 固定为 false。[S5][S6]

这里必须修正一个容易夸大的判断：实际 ProcessRunTool 已有 `ProcessTreeGuard` 和 kill_on_drop，不能说 abort 后必然没有树清理。问题是当前协作取消、任务 join、清理完成和用户 ACK 没有在这条桥接路径上明确对齐。

复用已有取消与 runner：传递取消令牌、清理后再确认、继续按 turn/generation/证明 basis 拒绝迟到结果。然后把经过相关验证的 deferred 模式接到支持的 TUI 配置。旧 inline 模式可留给冻结实验显式选择，但不应以实验冻结为由让产品长期卡在旧 await 路径。

### 4.3 TUI resync：重建了“错误的一组运行状态”

`AppState::resync_projection` 的问题组合如下：[S7]

- 按修改时间升序排序后 truncate(16)，选到最旧 16 个文件，而非注释所说最新文件。
- 没有按当前 self.run_id 过滤就把所有 envelope.event 折叠到一个投影。
- 重放后，broadcast 中仍可能有已从磁盘折叠的事件；当前投影没有按 run+sequence 去重水位。
- 先完整读取再检查 32 MiB，不能限制读入分配；枚举阶段也不是只保留 16 个候选。

因此旧 run 的完成、消耗、任务可能污染当前显示，重复事件又可能重复计数。它不直接改动 TaskManager 或 Core，但会误导用户判断及后续诊断。

最小修复：优先当前 run 的精确 journal；确需枚举时使用有界候选并正确取新；先按 run 过滤、按持久事件序列折叠；重放与随后 live stream 对齐水位。live-only delta 与持久事件的序列语义要分别保留，不能盲目过滤所有序列相同的流片段。读取用有界流式方式；不完整或损坏返回可见 partial 状态，不宣称完整 resync。

相关回归用两条 run、重复事件和超过 16 个文件即可；不建设新 Chronicle。

### 4.4 Run Summary：required misses 被读成零

契约 `ContextMaterializationMisses` 序列化的是 `entries` 与 `omitted`；`total()` 是方法，不是序列化字段。Run Summary 却读取 `required_misses.total`，缺失时默认 0。[S8][S9]

标准事件因此在这个离线摘要里漏计 required misses。Runtime 完成门禁使用类型化方法，是另一个路径；不能把显示漏计夸大成整个完成门禁失效。

用实际 RuntimeEvent 序列化数据做一个回归，优先复用类型化解析；total 计算 entries.len()+omitted。混合 run 的分隔、坏行与整文件读取也应在同一个只读函数中保守处理，未知不等于零。

### 4.5 Shadow Frame：不要从诊断 manifest 推导产品节省

`ZoneAccumulator.omitted` 同时记录重复删除和达到 zone cap 的省略；最后 `duplicates_removed = evidence.omitted + memory.omitted`。放入 9 份不同正文，zone cap 为 8，也会得到一个“duplicate”。[S10]

另外，gate_shadow_frame 计算 mandatory expected digest 后没有比较正文 digest；`omitted > 0` 的宽泛豁免也不能证明每个必需项目的准确覆盖。current_interpretation 被分类为 OperatorBoundary，而它可以来自自主进度提案；这应区分权威出处，不把“Runtime 保存了它”当成“操作者亲自下达”。这仍是影子分类问题，不是已证实的产品 prompt 提权。[S10]

该 manifest 也没有完整表示系统、工具 schema、当前 TurnFrame 和恢复正文；240/600 字符预览与完整正文的 token 成本不能混作实际请求预算。

最小修复：分别统计重复、数量省略、预览截断；明确 full-source / preview / actual-rendered 口径；必要项目检查值及出处。保留 shadow-only，不直接翻为正式 prompt，不用它作为 F1–F4 的通用前置门禁。

### 4.6 两个仍在的直接小错误

消费 ACK：`stamp_consumed` 仍位于 `debug_assert!` 内，优化构建默认可能完全不执行这个表达式。应先执行实际 mutation，再做调试断言，保留前置所有权校验。[S11][R2]

PromptRequired：强制候选先选入后，普通候选轮只跳过 Pinned，没有跳过之前已选的非 Pinned 条目。应在扣预算前判重，不在最终数组简单 dedup。[S12]

只跑这两个相关定向回归，消费更新覆盖 release；不要把它们扩大成全仓新一轮重构与质量治理项目。

## 5. 核心资产的下一版：先修表示，再选算法

以下均为本轮建议，不是当前源码已经实现的能力，也没有在本仓库证明性能提升。

### 5.1 最重要的最小契约：EvidenceSlice

现在去重依赖 `path@revision`。它能说明文件版本相同，却不能说明第 201–300 行已经覆盖第 1–100 行。[S13]

建议在既有契约里表达：

```text
ResourceVersion = resource locator + 整资源 revision
EvidenceSlice   = ResourceVersion + 实际返回范围 + 正文 digest
                + 表示类型 + partial/truncated 状态
```

整文件 revision 继续做编辑 CAS。正文覆盖需要同版本、明确范围包含、表示兼容；否则不省略。没有范围的旧记录是 Unknown，不是完整文件。同版本互补片段可以共存；版本变化可以使旧内容失去“当前文件”资格，但历史来源仍应可追溯。

这一改动不要求新增证据数据库、统一知识图谱或重写所有 Context 类型。第一步甚至可以先收紧 `omit_selected_file_body` 的条件，消除不安全省略，再逐步传播片段字段。

实际 renderer 应产生同一份曝光账目：body / descriptor / omitted，以及预算裁剪原因。ACK 从最终请求的实际曝光产生，而不是候选数组；它只表示请求包含了内容，不表示模型理解或用对了内容。

### 5.2 工作焦点：不要让原始总目标长期压过当前子目标

当前 scorer 对 current_query 和 goal 的 lexical_overlap 取最大值。一次自主长任务内，很多局部工作改变，但原 goal/current_query 可能保持很久；与总目标沾边的旧材料因而持续有分。该风险来自代码结构，实际收益仍需实验，不是已量化的性能缺陷。[S14]

先增加一个短的工作焦点投影，复用 `next_action`、活动 open_loop、最近可信 resource touches。用户目标和约束仍是稳定边界；模型计划是 advisory，不提供验证权威。

候选集继续使用现有索引：明确引用、当前任务/作用域、相关资源、结构化条件；不用新建 planner。先缓存 query tokens，避免每个 item 重复分词。中文、camelCase、snake_case、路径与符号分别加有限辅助 token，并保留原始精确身份。不要让辅助模糊 token 接管 ID/版本匹配。

### 5.3 选择算法：硬约束 + 依赖组 + 新增收益/新增 token

Pinned/PromptRequired 先由硬约束处理。可选材料可以比较以下贪心策略：

```text
priority(group | selected)
  = [当前子目标相关性 + 未解决问题覆盖 + 新信息收益 − 重复惩罚]
    / max(1, 新增渲染 token)
```

group 包含候选及其必要正文依赖；已经可见的片段不重复扣费。弱实体关联、引用溯源不自动变成强驻留依赖。冗余惩罚可先用精确身份、范围和 token overlap，不需 embeddings。

这是可解释的启发式，不声称最优解或近似保证。现有打分保留为对照。预算中未被使用的依赖预留可以借回；昂贵材料可以选择正文片段、摘要或描述符，但“要求正文”的 obligation 不能被描述符悄悄满足。

必要材料放不下时显式 required miss / 收窄当前任务 / 安全让出，不将“窗口有限”伪装成“信息已满足”。

### 5.4 压缩算法：可恢复 observation masking 是强基线

建议顺序：先保存精确工件，旧工具正文转为可恢复指针；当前焦点、约束、失败和必要证据保持稳定；阶段关闭且确有语义增量时再做局部摘要。无需每轮重新总结整个历史。

The Complexity Trap 在其 SWE-agent / SWE-bench Verified 实验中发现简单 observation masking 可以与模型摘要竞争，并展示成本优势。这支持将它作为本仓库对照，不代表本仓库已经得到相同百分比收益。[R3]

Anthropic 的工程资料也强调按需取回、结构化笔记和压缩的组合；可以借鉴方法，但平台经验不是此仓库的实验结论。[R4]

最小比较只需现有动态策略、可恢复正文遮罩、关闭阶段的局部折叠。使用已有任务与用量记录，计算完整任务成本，包括压缩调用、重读、恢复和失败修复；不要只比较一次 prompt 长短。

### 5.5 GC：以事件和字节预算维护，而不是换一个万能分数

当前已具备分轴状态、mark/sweep、Warm 外置、8 路 Store I/O、gc_work_batch 与游标，以及 plan→I/O→commit 的串行操作门。[S15][S16]

后续应原地迭代：

- 脏对象优先，复用 catalog dirty 和事件影响范围；游标保留作公平兜底。scope close 等安全相关失效必须及时生效，不等待后台扫描。
- 用户回合、event_seq、GC generation、lease 的时钟分别定义。不要将额外 preview/维护调用变成事实提前失效的原因。
- 每次安全点同时有工作量和字节预算，时间作为运行保护/遥测；正确性不能取决于机器恰好快慢。
- 在确有大规模到期扫描成本时，再加简单到期堆或桶；500 条以内的小集合不必用复杂 timer wheel。
- 注意力变化的迟滞/最短驻留可作为防抖实验；不能延迟安全失效，也不能复活语义终态。

语义失效由明确替代、可信验证或显式策略决定。低相关、空间压力和缓存低频主要影响注意力/位置，不能自动成为事实已错或错误已修复的证明。lease 跨 Resident/Warm 的保护应有同一解释。

不要仅因为 state lock 在 I/O 前释放，就宣称控制面延迟已经有界：目前 op_gate 仍串行化整个操作跨度，Actor 自身也可能等待该操作。移出耗时工作需要携带版本、取消、结果提交边界，不是删锁或随处 spawn。

### 5.6 缓存：SIEVE 适合可逆正文缓存，不适合决定记忆真假

SIEVE 使用插入序列、扫描指针和 visited bit，命中不必把对象挪到链表头。它原本针对 Web cache 的研究，不是 Agent 正确性算法。[R5]

适用候选：Warm 或恢复正文 cache 中的可选项，且成本以 bytes 计。必要证据、有效 lease、Pinned 不参加普通淘汰。未可靠外置的最后一份正文也不能由 cache policy 删除。

TinyLFU 是准入策略，比较近期访问频率，判断新对象是否值得挤掉候选；当轨迹显示大量一次性读占据正文缓存时再考虑它，先不与 SIEVE 等全部组合上线。[R6]

现有 Warm cap 为 256 项；没有测出问题时，继续简单策略很可能更划算。评价应包含恢复字节、延迟、反复恢复率和维护开销，而非只看命中条目比例。

### 5.7 搜索：保留现有目录，把相关性优化接到最终排序

当前不是纯 grep 或无索引：ContextCatalog 覆盖各驻留位置，TextIndex 做有限倒排、词覆盖、稀有度、唯一前缀；结果使用有界 top-k heap。raw Tool/File 正文在 context search 中是 Fetch-only，避免 stdout 成为任意记忆命中。它们的内容搜索与 workspace grep 是不同用途。[S17][S18][S19]

值得升级的地方：

1. 精确 ID/locator/版本先行，任务/类型/标签过滤先行。显式历史查询不能被默认活动任务过滤掉。
2. 词法候选覆盖/稀有度目前在最终 SearchEntry 排序中没有保留；最终仍是 entity/path 命中与时间等排序。这是当前明确的旧策略，不是“已经有 BM25”。先让相关性特征进入最终排序，再评价换分数。
3. 若使用 BM25 风格字段分数，应分别处理路径、实体、标题、语义正文，维护词频与长度统计。当前索引只有去重 token，完整 BM25 不是替换一行公式。原地扩展有界索引即可，不需要 Java/Lucene 服务。[R7]
4. Stored 语义正文的索引前缀/摘要不足时，当前会计划至多 256 次冷读并以 8 路执行；超限明确报错，不是静默漏召回。仅加长 query 不一定减少候选，task/kind/label 结构过滤才直接收缩这个集合。[S19]
5. 若冷读成本在真实轨迹上显著，再建立可重建的分段词法索引、或新增明确的 fast/complete 查询模式。fast 的部分结果必须附未完成范围和继续方式；不能把 partial 变成“无匹配”。修改 API 是前瞻设计，不改变当前完整性承诺。
6. workspace grep/list 的结果分页、扫描终止、跳过大文件/二进制/读取失败应分开。context search 的不完整索引机制已经存在，不能笼统说整个搜索系统没有这类设计。

建议观测点：search 的冷读次数/字节、精确资源命中、最终有用片段位置、重复 fetch、必需证据可恢复性。先用真实失败/查询和已有事件，后续有模型质量优越性主张再做多任务对照。

## 6. 调度：一位状态提交者，几类有界工作

概念上将安全点内工作按以下优先关系组织，不新建独立 Scheduler crate：

```text
取消 / 撤销 / 停机
  → 已进入的不可拆提交屏障完成并结算
  → 本轮必需证据与当前证明
  → 下一次模型决策
  → 脏对象维护的有限配额
  → 可选摘要、预取与索引整理
```

取消优先不代表把正在原子提交的副作用砍成半笔。异步工作只返回带 task/turn/generation/资源版本的结果，Actor 核验后提交；不获得第二份 TaskManager 或 effect 权威。

对一般进程和验证，取消令牌、树清理、reap、ACK 形成完整链；EOF 修复只是这个链最小且直接的一个切片。

维护使用有界批次，并有防饥饿兜底；索引可合并更新，摘要可丢弃过期结果，但权限变更、必要证据失效不可延后。缓存 I/O 与昂贵摘要的相同版本工作可合并，不因为连续多个触发重复做同一份工作。

**先修具体等待点，再抽象调度。**当前并不需要优先级队列框架、分布式任务系统或并行 TaskGraph。

## 7. 功能路线：原工单继续，不另发明阶段

| 顺序 | 交付物 | 本轮补入 |
|---|---|---|
| D0 | CURRENT + NEXT_TASKS 唯一活动入口 | 移出 Immediate M15 旧指令；不扫全历史 |
| F1 | /continue、忙时单槽队列、错误可见 | ACK release、PromptRequired 判重仍随此小修 |
| F2 | /work、/plan、TaskAnchor 短清单 | 进度只是 advisory；不做 Step/DAG 权威 |
| F3 | 有限执行段、可取消证明 | 两工具 EOF 等待；取消令牌与清理确认；产品 flag |
| F4 | /review、可理解结果、可信状态 | TUI 当前-run重放；离线 misses 字段；不把旧修改算 Agent |
| F5 | 多文件片段与可恢复记忆、搜索覆盖 | 保守片段去重、相关性与索引优化的明确接入点 |
| F6 | 修 bug、加功能、重构中断续跑 | 复用真实产品配置和已有记录，不新建总门禁 |

F1/F2 可以先合并和试用，不需要等整个核心算法研究结束。F3 相关阻塞在演示其取消能力前修好，不能以“主线功能优先”为由谎称可取消。Shadow 统计修复、缓存实验不阻塞这些产品入口。

之后最直接的新功能候选是**非交互入口**：复用 ComposeConfig / RuntimeHandle，同一工具/权限/Context/恢复路径，提供 prompt/stdin、有限预算、JSONL 输出与诚实的退出状态。无人可审批时显式阻塞或拒绝，不隐含全允许。它能服务脚本、编辑器及受控自用开发，不必先做 daemon 或新通信平台。

再之后按真实需求选择编辑器接入、检索增强或受控自改进。受控自改进可以先在隔离分支产出补丁，由既有检查与独立审核决定合并；不需要先建设完整 Chronicle/TaskGraph，更不能让同一 Agent 放宽验收并自批晋级。

## 8. 文档应怎样改变

当前 ROADMAP 同时写 M15 closed 与 Immediate M15 route，并仍要求选新候选、重做预检/窗口。[S20] 问题不是“文档不够”，而是旧命令仍在活动入口。

保留 AGENTS 的架构/安全不变量，缩短其默认阅读负担；CURRENT 只描述当前状态与下一条工单；NEXT_TASKS 维持 D0、F1–F6；ROADMAP 用产品结果排序。STATUS 只索引，AUDIT_TODO 将问题按当前路径和条件分流。架构、安全、恢复、兼容文档按需查阅，不为每个小功能通读。

旧 M15 规范、原始 evidence、已关闭调查正文原样保留，但移出默认导航/执行清单。上轮替换包已经提供这个结构，本轮只更新同一 NEXT_TASKS 稿，避免多出一套队列。

**冻结实验身份，不永久冻结代码。**旧结果继续绑定旧 SHA/配置/策略；新正确性修复和新策略使用自己的版本与记录。不能因为旧窗口冻结而禁止所有未来 GC/搜索修改，也不能把旧 PASS 借给新算法。

现有文档检查只承担相应链接、状态字段等检查，不应升级为“所有历史描述先完成形式化一致性证明，才能开发功能”。

## 9. 检查怎样服务主线

开发：与本次改动有关的最小回归 + 一次可见演示。确认过滤的测试实际运行，不把零测试通过当覆盖。

集成：使用现有 CI；权限、effect、恢复改动保留既有必要检查。没有新代码/新原因不要重复运行同一全量套件。

科学结论：只有宣称新策略更好时才需要跨任务、固定模型配置、保留所有失败的对照。三个产品走查不是算法优越性的统计证明；反过来，算法实验尚无结论也不能阻止已验证的 /continue 接线交付。

本轮未运行 cargo，也未执行真实 provider coding task。因此报告不提供新 PASS 率，不声明任何建议算法已胜过当前基线。

## 10. 交付与使用

- `REVIEW.md`：本报告，研究/审查参考，不作为新的全局前置门禁。
- `READ_COVERAGE.csv`：本轮正文获取范围；不代表全仓文件清单。
- `audit_state.json`：基线、远端 CI、执行与未执行项。
- `pipe_eof_probe.json`：实际 OS 前提探针结果。
- `replacements/docs/NEXT_TASKS.md`：在上轮同一脚本上补充 F3/F4，并说明研究候选不拦产品。待用户或 Coding Agent 合并，未推送。

下一次实际开发从 D0 最小切换进入 F1，随后 F2/F3/F4；不是先把本报告所有可优化点全做完。

## 参考来源

下列 S 为固定源码/仓库证据；R 为外部一手资料。外部论文结果不等同于本仓库测量结果。

[S1] https://api.github.com/repos/Ye-Luo-0912/context-agent-prototype/branches/main
[S2] https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/33986702977
[S3] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/process.rs#L900-L1110
[S4] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/shell.rs
[S5] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-compose/src/proof_verifier.rs
[S6] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-tui/src/main.rs#L180-L205
[S7] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-tui/src/state.rs#L235-L290
[S8] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-contracts/src/context.rs#L1570-L1695
[S9] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-replay/src/run_summary.rs#L1-L172
[S10] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/frame.rs#L1-L670
[S11] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/engine.rs#L1518-L1614
[S12] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/materializer.rs#L283-L351
[S13] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/prompt.rs#L940-L1028
[S14] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/policy/simple.rs
[S15] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/gc/full/mod.rs
[S16] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/engine.rs#L1-L320
[S17] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/index/catalog.rs#L1-L725
[S18] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-contracts/src/search.rs#L1-L395
[S19] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/store.rs#L1-L750
[S20] https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/ROADMAP.md#L1-L122
[R1] https://docs.rs/tokio/latest/tokio/macro.select.html
[R2] https://doc.rust-lang.org/std/macro.debug_assert.html
[R3] https://arxiv.org/abs/2508.21433
[R4] https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
[R5] https://www.usenix.org/publications/loginonline/sieve-cache-eviction-can-be-simple-effective-and-scalable
[R6] https://arxiv.org/abs/1512.00727
[R7] https://lucene.apache.org/core/9_12_1/core/org/apache/lucene/search/similarities/BM25Similarity.html
