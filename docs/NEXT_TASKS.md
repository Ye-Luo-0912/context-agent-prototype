# 可执行任务队列

有效范围见 [CURRENT.md](CURRENT.md)。本文件只保留本阶段仍需动作的任务；历史缺陷描述、验证日志和已关闭细节只链接到原回执，不复制正文。任务依据：[下一阶段审查](reviews/2026-09-14-next-stage-review-4aaa8bea/REVIEW.md)（T 编号）＋[续审](reviews/2026-09-15-continuation-review-258eb4eb/REVIEW.md)（S 编号；R1–R6 为其发现；T1–T7、S1–S4 已关闭）＋[4f6eb7ff 审查](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)（V 编号，其 NEXT_ACTIONS 与覆盖表见同目录）。

## 接手规则

先核对当前分支、HEAD 和未提交 diff（并行分支在飞）。MERGED 只说明代码进入目标分支，不代表 CI 或真实供应商验收通过。本轮 A=执行核心/工具，B=上下文/GC/搜索，C=平台/供应商 KV。共享 contracts/ModelInput/缓存契约由单一集成人维护。

**开工顺序：T1/T2/T3 并行（第一批）；T4/T6 并行（第二批）；T5 随组合点接入；T7 收尾；T8 条件任务。** 每片先写用户动作与目标反例，再做实现；可维护性边界（见审查报告「四个责任边界」节）随片交付，不另起全仓重写。

## S1 — 冷目录测试自锁（已关闭 2026-09-15）

`externalize_growth_demotes…` 曾在持有 state guard 时调用 `fetch_external`（内部重取同一锁），确定性自锁（CI run `34917761534` 两 job 取消）。修复：锁内只快照、guard 释放后经公开 API fetch、重锁核对；owner 断言改为集合比较（旧∪新 vs 热∪pending，不重叠，替代单纯总数）。cold_bounds 2/2、B2 3/3 绿、clippy 0。

## 第二批（S2 可并行修正）

### S2a — 最终装箱身份/覆盖一致性（B/C，agent-runtime）——已关闭（2026-09-15，`886e1d7f`）
required_item_ids 的清理在物理记录被移除时无条件执行（旧代码仅在记录 miss 时清理——异 ID 覆盖时 ID 残留导致 validate fence）。回归 `covered_required_id_leaves_required_item_ids_without_structural_failure` 红→绿。
`record_final_pack_drop` 允许异 ID 同路径/版本/范围覆盖被删正文且不记 miss；但被删 ID 仍留在 `required_item_ids`，最终 validate 报其缺失并中止——证据仍在帧内却结构性失败。修复：分开"物理记录是否仍存在"与"证据义务是否被覆盖"；由覆盖关系决定 miss 与义务承担者，不由"是否刚添加 miss"顺带决定身份清单。回归须走最终裁剪→覆盖计算→最终校验（含：同 ID 双层删一份、异 ID 覆盖、异版本不覆盖、真预算不足如实 miss）。不要清空 required IDs 或放宽 validator。

### S2b — 官方缓存 mapper 协议一致性（C，provider-openai）——已关闭（2026-09-15，`ea9c0e5c`）
declared 断点路径重写 string content 为 content-block 数组（`input_text` 块携带断点），与 legacy 单断点分支和官方文档一致；`function_call_output` 项的 `output` 同样包装为 content-block 数组并携带断点。`endpoint_shape_tests` 从差异 pin 翻转为正确形状断言。compose wire 验收 B0 断言更新为 ContentPart 形式。
declared 多断点仍放 input item sibling；旧单断点分支反而已是 content-block 形。按官方 Responses 文档（断点在受支持 content block 上）修正真实 mapper，fixture 断言正确形状；`endpoint_shape_tests` 现有的"差异 pin"测试翻转为正确形状断言。兼容网关方言（若确认）分开命名；未知能力端点继续不发送专属字段。这是本地可完成的协议修正，不等 T8 付费实验。

## 第三批（S3 主体能力）

### S3 — 固定预算冷目录闭环（B，context-simple）——已关闭（2026-09-15）
T4 一期/二期已有预算与降级，但"有界"仍是局部控制（续审 R3/R4）：(a) deadline 只在批间检查，单次读取不被剩余期限约束；(b) 批 take 按条数不按剩余字节预留；(c) per-id fetch/inspect 绕过热上限可无限扩大热表；(d) demote 跳过 pinned/无 claim，无可降级项时不报告背压；(e) 非空搜索命中丢掉 HydrationOutcome——1 命中/limit 20/大量未读页时模型看到普通结果无覆盖缺口。收口方向：统一安装/读后驻留/降级/预算结算到同一 metadata-residency 入口；搜索结果沿引擎→服务→Core→模型正文传 hits+coverage+remaining+stop_reason（复用 HydrationOutcome）；固定预算下连续访问不同冷页仍可取正文且热资源受控或明确背压；续查真正推进到后续冷页（不反复撞同一满员热表）。命名收口：`hydrate_all_pending_cards` 已不保证读完全部 pending——改名或契约化（不追加历史补丁注释）。
落地回执：[S3_FIXED_BUDGET_COLD_DIR_RECEIPT](reviews/2026-09-15-continuation-review-258eb4eb/S3_FIXED_BUDGET_COLD_DIR_RECEIPT.md)。单次读取进剩余 deadline（超时保持 pending owner、`HydrationStop::Deadline`）；安装前按估算字节预留、装不下保持可寻址 pending；`ExternalMap::stamp_access` 保 claim（根因：get_mut 访问戳杀 claim 使读过条目不可降级）+ 统一 `settle_metadata_residency` + `demote_overflow` 类型化 `DemoteOutcome`/`hot_metadata_backpressure`；搜索沿引擎→Core→模型正文传 coverage+continuation（坏页不挡后续页、续查累积 skip 收敛）；`hydrate_all_pending_cards` → `hydrate_pending_cards_within_budget`。context-simple 414/0、clippy 0、7 条新反例红→绿。限制（wire 层 coverage、续查 token 不进 checkpoint、pending 目录仍随历史增长）见回执。

### S4 — T7 补进程边界与指令传递证据（A，agent-host）——已关闭（2026-09-15）
现旅程的同进程重组保留，但补两个证据：(a) 独立进程变体——用已有 host 二进制新 OS 进程（不同 PID）、确认前一进程退出、从磁盘恢复同 TaskId/lineage 继续；(b) 纠正传递证据——唯一标记的 steer 指令，脚本 provider 检查实际收到的请求确实包含该标记与任务约束（缺失即拒绝推进），恢复后的请求也断言包含正确剩余义务。不换真实付费模型。
落地回执：[S4_PROCESS_AND_STEER_EVIDENCE_RECEIPT](reviews/2026-09-15-continuation-review-258eb4eb/S4_PROCESS_AND_STEER_EVIDENCE_RECEIPT.md)。新测试 `host_process_variant`（named pipe/UDS 双入口）：真实 host 二进制两进程、`pid confirmed gone` 后新进程接管 stale host.lock、`work/restore` 携带被杀进程 RunId、同 TaskId 重聚焦且无效果重放；脚本 provider 纯函数门禁（缺目标/缺标记/恢复后缺剩余义务即拒绝），写入内容由请求体内实际送达的 token 派生。host 套件 31/0、clippy 0、进程变体 3 次重复全绿。限制（同进程旅程保留、watchdog 全路径不声称、unix 入口未本机运行）见回执。

## 第四批（V 系列四切片；V1 先行，其余可并行）

审查基线 `4f6eb7ff`（报告：[REVIEW.md](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)，行动与停止条件：[NEXT_ACTIONS.md](reviews/2026-09-16-review-4f6eb7ff/NEXT_ACTIONS.md)）。共同主线：分页只改变驻留位置，不改变记录的语义身份与保护义务；取消只改变执行结果，不抹掉已知成本。共享 ContextEngine/service wire/ModelRequest 契约由单一集成人维护；行为修改与机械移动分开提交；定向测试后跑既有相关跨 crate 集成，合并沿用现有 CI。

### W1 — 逻辑 owner 与冷页解析（B，context-simple；含 V1＋V2）——已关闭（2026-09-16，`c0923af2`）
用户动作：历史已外置/分页后，Agent 仍可正确要求这份证据进入下一次请求；运行 GC 不会让未读卡片失去有效 scope。
- V1（P1）：`retire_closed_scopes` 的 referenced 集合只枚举 heap/Warm/写入重试表/已加载 external/active scope，未加载 pending 卡片中的 scope 引用不可见——退休可移除仍被未读冷页引用的 scope，`hydrate_card_for` 读回时因 scope 不存在而消费定位行，破坏可达性/恢复目录。修复：把元数据/引用闭包完整性传入退休许可——未证明闭包完整时不退休未知冷页可能引用的节点；不依赖完整引用信息的内存 GC 照常。不许删 scope 校验放行坏结构；不许全历史加载替代分页。长期可沿目录维护 scope 引用计数（须与分页 owner/迁移/checkpoint 原子一致，不建第二份任务真相）。
- V2：`materialize`→`plan_required` 只查已加载索引，pending 卡片不在解析路径——同一正文 `fetch_external(id)` 可读、声明成 PromptRequired 却报 Missing。修复：必需正文规划前对有界 required refs 做目标解析（精确 ID 直接定位卡片；路径/实体沿冷目录）；区分 不存在/读取失败/策略排除/因预算未解析，不压成 Missing，不全历史 hydration。
- 回归：卡片保持未加载→GC 触发退休→按 ID 读取→checkpoint/restore，核对正文、scope 关系、owner 集合（证明目标卡片本轮未加载，不得靠 fixture 顺序巧合进热表绕过反例）；required 目标位于 restore 首批之后＋固定热预算＋精确 URI 直接 materialize。
- 停止：同一正文在已加载与分页状态下语义保证一致；正常退休仍可收敛（释放最后一个真实引用后才退休）。
落地回执：[W1_OWNER_AND_REQUIRED_RESOLUTION_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W1_OWNER_AND_REQUIRED_RESOLUTION_RECEIPT.md)。闭包不完整时有界探测收集 pending 卡片 scope 引用、预算耗尽诚实推迟退休（`scope_retirement_deferred`）；required 解析走 per-id lane＋有界目录扫描，原因类型化 Missing/Corrupt/IoFailed/UnreadColdPage。context-simple 425/0，4 反例红→绿。限制：pending 超探测预算时退休持续推迟（诚实保守）；foreground optional_misses 未做未读区分。

### W2 — 原子搜索结果与 continuation 生命周期（B/C；含 V3＋V4＋V5）——已关闭（2026-09-16，`c0923af2`）
用户动作：部分命中时 Agent 知道尚有未读区间并能在固定预算内续查；恢复/目录变更后旧游标不能错用。
- V3：`ContextServiceAdapter::search_external` 只返回 `Vec<ExternalizedContext>`，未覆盖 `last_search_coverage`/`search_external_continuation`，继承默认 complete＋忽略 token——service 模式退回「普通完整结果」。修复：一次搜索原子返回 hits+coverage+observation+continuation/视图身份（不靠调用后再读可变 `last_search_*` 旁路）；旧服务无该能力时明确 Unknown/Unsupported，不默认 complete；经既有协议版本/能力协商迁移。
- V4：`record_search_coverage` 在 query_key 相同时无去重追加旧 `covered_ids`，普通搜索不持 token 也可持续追加同一热窗口——固定历史下重复普通查询即可让 retained state 随调用次数增长。修复：fresh（开始新遍历，不继承）与 resume（验证过的遍历）分开；覆盖状态去重、链级条数/字节边界；超界返回显式状态。
- V5：`restore` 替换 State 但不清理 State 之外的续查状态；token 编号由 slot 尾数推导、链完成后清空重开——旧 token 可在原位 restore 后或新链 ABA 命中。修复：成功 restore 使旧 token 失效（拒绝 restore 不动现有合法状态）；token 用进程生命周期单调 nonce/不透明身份并绑定目录/恢复代际。
- 回归：固定历史上重复普通搜索，retained state 不随调用次数增长；固定预算续查每次推进不漏页；同一冷热 fixture 下 in-process 与 service 模式的 非空不完整/空不完整/续查到末页/过期 token 语义一致；视图 V1 签发 token→恢复 V0→旧 token 不得把 V0 未搜索内容当作已覆盖。
- 停止：一次查询结果自带 coverage；fresh/resume/restore 一套生命周期规则。
落地回执：[W2_ATOMIC_SEARCH_AND_CONTINUATION_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W2_ATOMIC_SEARCH_AND_CONTINUATION_RECEIPT.md)。`ContextSearchResult{hits,coverage,observation}` 原子返回＋新 wire op `SearchExternalReport`（握手协商 `context-search-report.v1`，未协商显式 Unsupported）；fresh 不继承、resume 去重＋链级 16384 条上限、restore 失效全部旧 token、nonce＋恢复代际防 ABA；`last_search_*` 旁路诚实化为 Unknown 且 service/Core 均不读。context-contextcore 9/0、agent-context-service 11+20/0，红-first 7 条（变异恢复法）。限制：已消费 token 重试采稳定降级 fresh；covered 集仍是累积 ID 集非目录快照游标（审查长期方向）。

### W3 — 工具结果缓存断点块类型（C，provider-openai；V6）——已关闭（2026-09-16，`9b176df0`）
用户动作：显式标记可复用工具结果时，请求使用合法输入内容块，不发错误类型或未确认 fallback。
- `build_responses_wire_request` 的 function_call_output 分支把 output 包成 `output_text` 块——官方 Responses 缓存断点只支持 input_text/input_image/input_file；另有未确认的 input-item sibling fallback 残留。现有 `endpoint_shape_tests` 工具用例只断言"数组内有断点、顶层没有"，未查块类型（函数名/注释还与断言相反）。
- 修复：受支持 input 块类型化映射；无法合法承载断点时剔除 hint 并记录原因（或拒绝该缓存计划），保留真实消息与工具配对；网关方言显式独立命名。普通文本 input_text 已修部分不重做。
- 回归：fixture 断言块类型/位置/消息展开后的对应关系（不只字段存在）。本地无需付费可完成；真实接受/命中/净费用仍是 T8 条件实验，不互相替代。
落地回执：[W3_TOOL_OUTPUT_BREAKPOINT_BLOCK_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W3_TOOL_OUTPUT_BREAKPOINT_BLOCK_RECEIPT.md)。function_call_output 包 `input_text` 块（官方 multi-turn 示例同形）；未确认 sibling fallback 全删；`place_declared_breakpoint`×`BREAKPOINT_BLOCK_TYPES` 单一映射 seam，无法承载时类型化剔除＋记录。provider-openai 155/155。

### W4 — 取消不抹掉已知用量（A/C；V7）——已关闭（2026-09-16，`4fa2d8a2`）
用户动作：失败调用已报告费用→退避等待中用户取消→账目仍保留已知数值，执行状态仍是取消。
- 现状：backoff cancel 分支把 known_usage 写 CallStage 后返回普通 `AgentError::Cancelled`；生产 observer 是 `JsonlRetryObserver::from_env()`——未设 `OPENAI_RETRY_METRICS_FILE` 时 append 直接返回；Runtime 把 Cancelled 映射为不带用量的 `OperationOutcome::Cancelled`。已知信息被降级为无正式去向。
- 修复：沿既有模型 operation 结算入口把 outcome 与 usage 正交表达——取消仍保持 Cancelled 分类与安全屏障/代际隔离，已知 usage 走必有的结算通道，JSONL 只留诊断副本。不许用 `FailedWithUsage(Cancelled)` 换返回值而不调调用链（会把取消误分类为失败）。
- 回归：Compose→Retry→backoff cancel 全链、metrics 环境变量不存在：已知用量保留、取消分类不变、未执行重试不多计、未知字段不补零、同次 SSE 累计快照不重复相加。
落地回执：[W4_CANCEL_SETTLES_KNOWN_USAGE_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W4_CANCEL_SETTLES_KNOWN_USAGE_RECEIPT.md)。`FailedWithUsage{source:Cancelled}` 携带（裸 Cancelled 不变）、`OperationOutcome::Cancelled{known_usage}` 守卫臂结算、屏障前按真实计数记账＋fence、迟到 stale completion 经有界 FIFO 一次性补记；observer 降级为诊断副本。全链回归在 env 不存在下断言正式账目。限制：maintenance lane 取消仍 unknown（token 永不取消）；迟到成功不补记（既有设计）。

## T1–T7 完成记录（历史）



### T1 — 统一最终装箱与发布（B/C 共享，单一集成人）

结果：预算不足时优先保留真正必要的证据；模型实际输入与观测、覆盖和缓存边界一致。

修 R1（分区内选择可选项：selected 只剩必需正文时，会先于 foreground 中的可选大正文被裁，制造本可避免的 BudgetExcluded 必需缺口）＋ R2（撤销 settlement 投影重组请求后 input_total/packing_total 未重算，计数来自旧请求）。入口：`actor/model.rs`、`prompt.rs`、model/model_cache 契约。先修两缺陷，再沿 ModelRoundPlan/ModelInput 抽取稳定纯装箱函数：统一候选优先级→一次生成最终消息/工具/覆盖/计数/缓存计划。Actor 继续负责调度、身份检查与发布。

验收：R1 两分区反例（selected=必需小正文＋foreground=可选大正文、总输入略超预算→必需保留、无对应 required miss）；全必需确实超限时仍诚实报告；投影修订后计数与完整重算相等；无 over-budget 伪发送。停止：不改 Context 打分/GC 算法、不重做全部 prompt 文案、不新增事件数据库。

### T2 — 外置目录的增量维护（B）

结果：无变化的维护不反复付序列化和目录重建开销，冷恢复保证保持。

修 R3（零删除或仅延期的 storage GC 仍对全部 external 条目 take_all→replace_all，清空 card_hashes 并触发目录/索引重建；后续 checkpoint 可能重新序列化未变化元数据）。入口：`store.rs::commit_storage_gc`、`index/external.rs`。零删除零结构变更、仅更新必要观测；部分删除只处理真实成功/NotFound 的 id、保留其他条目有效 card claim；把针对单项的结构变更收敛到具名目录 API（状态迁移＋索引更新＋card claim 作废同入口），减少 get_mut 调用者隐式约定。

验收：已记录卡片的 N 条目→零候选 GC/不完整根延期→全部原 card claims 保持；部分删除仅对应 id 消失、后续 capture 不重新序列化幸存者；现有恢复根与 pending owner 反例全部保持。停止：暂不引入新数据库；真正冷驻留归 T4，不把 checkpoint 分片冒充内存分页。

### T3 — 有界进程与能力发现（A）

结果：长工具可持续轮询/停止，退出/输出/清理事实清楚；MCP 发现不超出声明预算悄悄成功。

修 R4（工具总数与总期限只在下一次循环入口检查：末页追加后可直接返回成功、末页请求可跨过总期限）＋ R5（MCP 的 reap helper 忽略 `ProcessReapOutcome::Unconfirmed` 并清空 supervisor，上层失去"清理未确认"的类型化事实）。入口：`mcp.rs::list_tools_with_cancel`、`supervisor.rs`。总量/累计字节检查放到接纳与最终返回前；剩余总期限传入实际交换边界；关键结算结果 `#[must_use]`，调用方明确传播/保留待结算/报告不确定性；游标循环用有界 seen set。Core 准入另有 MAX_TOOLS_PER_CAPABILITY=32——发现上限与准入上限是有意分层时须明确用户能否筛选。

验收：单页 513 工具无 nextCursor 仍拒；末页跨总期限仍拒；**用不同工具名或空页构造 cursor 循环**（不得让"重复工具名"拒绝抢先触发、实际未验证游标规则）；Unconfirmed 经可注入 supervision seam 传播。停止：不新增进程调度器、不扩展 MCP 协议范围替代已声明范围修复。

## 第二批（并行）——已全部完成合入 main（2026-09-15，T4 `1a851169`、T6 `dbde17e8`、T5 `9edd63f5`；各自本地全量绿＋clippy 0，远端 CI 待记录）

### T4 — 真的有界冷目录、搜索与维护工作（B）——第一期已完成（2026-09-15，`1a851169`）
每操作预算（items＋绝对 deadline）、类型化 `HydrationOutcome{complete,remaining,stop:Budget|HotCap|Unreadable}`、热上限（8192 条/32 MiB 软）落地；规模回归红→绿（5 倍超预算冷集合全程有界可继续）。第二期（持久冷目录分页、热/冷双向驻留、跨 crate typed 传播）沿原队列继续。原始任务：

结果：历史增长后，常用任务不因首次搜索或 GC 全量重水化历史而大幅停顿；未读区域始终可检索且不被误删。

沿现有 store/index/ContextEngine 实现持久目录或分页查询视图、有上限热元数据缓存、受保护 roots/pinned/dirty 条目的明确规则。先记录当前元数据规模与查询工作量再选实现，不先决定向量库或新服务。每操作分别定义 items/bytes/IO/绝对时间预算；超出返回可继续状态或类型化不完整，不把总工作量推给隐藏的 hydrate_all 循环。搜索区分"排名 Top-K"与"因未读页/故障覆盖不完整"（类型化差异）。`run_storage_io` 的派发后汇集模式使累积完成结果与候选计划总量无界——与单批总工作量设计一起收。

验收：明显大于热预算的冷集合；恢复/按 id fetch/搜索/GC 往返后热资源不随全历史永久增长；未读依赖闭包时延期删除；候选排序/结果稳定性有版本依据。停止：第一版只证明声明规模下的边界，不宣称无限历史、任意故障常数时延。

### T6 — 供应商 KV 的稳定布局（C，依赖 T1）——第一期已完成（2026-09-15，`dbde17e8`）
R6 已修（`rc2-`＋64 hex 摘要，一次性路由迁移已记录）；官方 content-block 形状与当前 item 同级形式的差异固化为 `endpoint_shape_tests` fixture（翻转断言归 T8 端点核验）。第二期（三层稳定布局落地、CachePlan 收敛）沿原队列继续。原始任务：

结果：连续请求复用有效、预计重复使用的前缀，减少无谓缓存写入，不延迟新事实或新指令生效。

先修 R6（路由键 `|` 拼接无转义可碰撞——`(a|b,c,…)` 与 `(a,b|c,…)` 同串；对带版本结构化 tuple 规范序列化后经 SHA-256/ContentDigest 生成定长摘要，不截断结构、不改每轮随机键；键编码变更即一次路由迁移，记录即可）。核对具体端点断点编码：本地捕获服务器不校验 schema，官方 Responses 走其文档定义的 content-block 形式，网关扩展单独声明；独立 schema/fixture 断言不复制 mapper 当前输出当"正确答案"。沿 EvidenceSplit/ModelInput 构建三层布局：稳定基座（稳定策略/工具契约/较稳定有效材料）→当前阶段证据（版本化正文与范围）→动态尾部（新检索/缺失/恢复/最新指令）；频繁编辑正文不强留稳定区。"epoch"须代表实际跨轮有效快照/布局规则，非每轮新块命名。派生布局字节计入内存预算，不持第二份无界历史；权限撤销/来源版本变化/用户移除要求/必需缺口/硬窗口即时失效。缓存计划收敛为经最终输入验证的单一 CachePlan（旧 prefix+tools 摘要 hint 与新裸断点列表不并行两套失效规则）。

验收：相同有效基座＋不同焦点/缺失/恢复信息保留相同基座；文件修改/工具撤销/任务切换确实失效；A→B→A 重组不冒称原有后缀自动命中；最终 wire 断点位置合法。可用无付费模型的请求序列完成；缓存收益归 T8。停止：不为命中率保留过时证据、不追求一次覆盖所有供应商。

## 组合点与收尾

### T5 — 一份有效产品配置到所有入口（A/C 共享）——已完成（2026-09-15，`9edd63f5`）
RuntimeServices 字段分组＋`from_parts` 唯一构造路径；`ComposeConfig::product_baseline` 一处定义产品公共默认，host 入口收敛为基线＋12 项差异；`build_context_engine` 文档诚实区分维护预算适用引擎（rolling 逐 pass 生效；dynamic 仅挂载决策）。特征化测试钉住冻结基线，无行为变化。原始任务：

结果：host、TUI/headless、SDK 与明确支持的 service 路径对当前策略、预算、权限、恢复和完成方式无隐含分歧。

复用 compose/services：产品与实验设置分组、共用默认构造和校验（RuntimeServices 的 new/try_new 重复初始化与实验 bool 汇合），入口只解析差异。正式 host 默认 Rolling 先保持；Dynamic 明确可启用 profile、回归范围与实际限制；正数维护预算/backoff 的适用引擎在 effective config 中诚实区分"不支持/未接入"与"默认"。验收：读取 effective config 与执行时实际选择一致；不支持的组合拒绝或显式报告；未知修改不自动重放；普通 final 与持久完成区别保持。停止：不换 GUI 技术、不加多工作区总调度、不任意改变公开 wire。

### T7 — 同一任务的完整后端开发流程（A 主持，三线共同）——已完成（2026-09-15，host_t7_journey）
一条连续轨迹（named pipe；unix 入口同款归 CI）：跨文件目标提交（幂等重试同 task）→ 脚本模型经 Runtime 真实 fs.write（wire 审批放行）→ 运行中 steer（Queued 槽，无第二任务）→ continue＋mid-flight cancel（类型化 TurnCancelled barrier）→ 正式 checkpoint → 宿主关闭 → **新 server 冷恢复同任务**（同 TaskId 重聚焦、run lineage 链接）→ 文件逐字节存活/无效果重放 → 继续完成 summary.md → 交付＋OperatorClosureOnly 操作员关闭（TaskCompleted 携带 final_output_digest）。故障变体：恢复后真实 fs 失败保持 Active/可重试、条件清除后同写入成功。host 全量 25 绿（t7 2），clippy 0。原始任务：

结果：一个真实跨模块代码任务在同一 TaskId、同一工作区、同一恢复 lineage 内完成——不是多组无关测试的成功行相加。

复用现有 host/headless 与测试 harness，受控模型决策＋真实工具/进程/checkpoint 文件驱动：提交→计划/验收约束→检索读取修改→超窗证据找回→验证→用户纠正→中断保存→退出进程→新进程冷恢复同任务→核对外部变化→继续完成→交付/等待操作员关闭。同任务遇慢维护或临时冷页故障不丢指令/owner；已发生效果不重放。service 模式按实际引擎能力作为明确变体。验收：最终产物、任务身份与输入 lineage、未决效果、清理状态、有效配置、证据与费用完整性均可核对；允许复用既有断言，不得拼接不同任务的成功行。停止：一条代表性综合流程及关键故障变体完成后即回功能开发，不变成无限新增门禁。

### T8 — 真实模型质量与任务全成本对照（C，条件任务）

有授权预算和凭据时：固定起点、任务、产物 oracle、模型/端点/profile、重复策略与缓存冷热条件。任务成本＝主调用全部尝试＋维护调用全部尝试＋工具＋适用存储；同 attempt 多 usage 快照不重复计、不同 attempt 不遗漏；普通输入/缓存读/缓存写/输出按供应商口径正规化，缺测独立记录不补零。比较**验收质量保持时的每任务净成本**（含缓存重写、维护开销、重复证据读取、无效验证），不是单看 cached ratio 或输入 token 数。接线完成/API 接受/实际命中/质量保持/费用下降分别给结论。无预算/凭据保持 NOT_RUN，不阻塞客户端接线。

## 已关闭（链接）

前阶段全部切片（文档入口迁移、B1–B3、B2、A1–A3、C1/C2、阶段旅程）的回执链接见 [CURRENT.md](CURRENT.md)「上一阶段成果」；正文不在此复制。

## 维护性验收尺度（随片适用）

- 修改同一条规则需要同步改几个位置？是否仍依赖调用者记住隐式约定（如 get_mut 不得动某字段）？
- 关键结算结果是否可被静默丢弃（`#[must_use]`/类型化传播）？
- 测试能否明确证明目标行为——反例不被其他拒绝分支抢先"证明"（如重复工具名抢在重复 cursor 之前失败）？
- 故障注入放 test-only seam 或隔离树；不在共享工作树改生产代码做红检查后靠记忆恢复（grace RED_CHECK 事故的教训）。
- 行为修改与大范围文件移动分开提交；不以拆文件数/测试数/覆盖率百分比为产物。
