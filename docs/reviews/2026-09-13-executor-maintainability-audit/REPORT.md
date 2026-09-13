# M18 第三轮：执行者 LLM、长期运行与可维护性续审

日期：2026-09-13。源码基线：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d` 加本轮开始时的未提交工作树（124 个已跟踪修改文件、20 条未跟踪状态记录；目录记录不等于文件数）。源码身份见 [source-start.json](evidence/source-start.json)。

本轮交付是审查、证据与三份后续任务，产品代码未改。只使用两个只读子 Agent；主 Agent 负责 Context/GC、跨进程 Context 和结果复核。不会把前序局部测试通过当作长期稳定、远端 CI 或真实降本的证明。

## 结论

共 14 项（4 P1、10 P2）。9 项通过本轮隔离探针观察到反例；5 项为已复核调用链的源码结论，未运行相应动态回归。R3-08 是已知未完的 CTX-8 接线，其余为新反例或当前修复的具体残余，不整体重开前序工单。

现有基础可以继续复用，但若干修复仍只在局部入口成立。最需要先处理的是：服务式 Context 漏传恢复保护根而删除旧证据；scope 退休与读回组合产生不可恢复的检查点；自然语言启发式仍会永久撤销兼容要求。长期资源与成本问题也应沿原切片接完，不能以 Pending 容器有上限、尾部结果有上限或记录了 usage 来替代组合验收。

可维护性工作的着力点是减少已经造成分叉的规则：正文位置解析、当前元数据合并、恢复根枚举、调用预算与费用事实。文件较长、存在 `clone` 或多一个辅助函数本身不列为缺陷；不为行数指标另造抽象层，不新增调度器、第二状态库或算法研究阶段。

执行顺序统一放在 [NEXT_TASKS.md](../../NEXT_TASKS.md) 顶部；三个任务包为 [A 上下文与 GC](TASK_A_CONTEXT_GC.md)、[B 执行核心与恢复](TASK_B_EXECUTION_RECOVERY.md)、[C 成本与接入](TASK_C_COST_CONNECTIVITY.md)。仍属 M18。

## 阅读与证据边界

- 全量盘点 507 个源码/测试/构建与根基线文件，其中 452 个 Rust/C#/AXAML 文件；对 505 个适用文件执行词法风险扫描。盘点含测试、夹具和构建脚本，**不表示 298,469 行逐行审阅**。
- 深读集中在执行者获取/保留证据、四种正文 owner、语义终结、GC、恢复、费用、事件与连接生命周期。具体覆盖见 [COVERAGE.md](COVERAGE.md)。搜索匹配只是导航，最终发现均追到调用方或实际反例。
- [context-probe](evidence/context-probe/src/main.rs) 直接依赖当前产品 crate；部分案例用通过正式 `restore` 校验的 checkpoint 设置 owner 状态，再运行真实方法。另有真实 scope 循环、真实输入和真实 `agent-context-service` 子进程。它们是有界反例，**不是完整产品旅程或性能基准**。
- [context-probe-results.jsonl](evidence/context-probe-results.jsonl) 保存九个反例观测。命令 exit 0 表示探针完成，输出中的缺陷仍然存在，不能将其标成修复 PASS。

| 编号 | 级别 | 问题 | 证据 | 归属 |
|---|---|---|---|---|
| R3-01 | P1 | 服务 Context 漏保留根，reconcile 删除旧 checkpoint 正文 | 真实子进程反例 | B / EXEC-9 |
| R3-02 | P1 | 退休 scope 被旧正文带回，checkpoint 自身不可恢复 | 引擎反例 | A / CTX-10 |
| R3-03 | P1 | 改超时日志误撤销超时时长要求 | 真实 ingest 反例 | A / CTX-2 残余 |
| R3-04 | P2 | 退休环丢最新事实，完成语义依赖可淘汰环 | scope 循环反例＋后果链 | A / CTX-10 |
| R3-05 | P2 | Pending foreground 漏正文 | 引擎反例 | A / CTX-11 |
| R3-06 | P2 | Stored 材料化丢当前 owner 元数据 | 引擎反例 | A / CTX-11 |
| R3-07 | P2 | 无效候选消耗召回额度 | 引擎反例 | A / CTX-12 |
| R3-08 | P2 | 背压仅上报，总驻留继续增长 | 持续存储故障反例＋Runtime 调用链 | B / CTX-8 接线 |
| R3-09 | P1 | restore 与停靠的终局事务未隔离 | 源码核实，门控回归未跑 | B / EXEC-10 |
| R3-10 | P2 | 第二个 checkpoint 覆盖 gc_work 槽位 | 源码核实，门控回归未跑 | B / EXEC-10 |
| R3-11 | P2 | 冷结果查询全日志扫描阻塞 Actor/journal | 源码核实，规模回归未跑 | B / EXEC-8 残余 |
| R3-12 | P2 | 失败出口丢已收到的 provider usage | 源码核实，SSE 回归未跑 | C / COST-7 残余 |
| R3-13 | P2 | 未发送候选使相同压缩请求绕过退避 | 真实 Rolling＋本地失败压缩器反例 | C 协调 A / COST-8 残余 |
| R3-14 | P2 | GUI 渲染丢弃队列吞尚未累计的成本事实 | 源码核实，dispatcher 回归未跑 | C / COST-9 |

## R3-01 / P1：服务式 Context 把“未实现根解析”当成“完整空保护集”

**触发与源码。** `ContextPolicy::Service` 是真实 compose 入口（`crates/agent-compose/src/lib.rs:149`）。`ContextServiceAdapter` 只覆盖普通 `storage_gc`、`reconcile_store`、checkpoint/restore（`crates/context-contextcore/src/adapter.rs:185–194,270–276`），未覆盖 `checkpoint_recovery_item_ids` 及两种 protecting 方法。公共 trait 默认解析返回空集（`crates/agent-contracts/src/context.rs:3140–3142`）；默认 protecting 在 `complete && empty` 时转调无保护方法（同文件 `3078–3079,3119–3120`）。Runtime 的保留检查点枚举调用这个 callback（`crates/agent-runtime/src/actor/restore.rs:389`），无法发现保护根其实未被解析。

**实际反例。** 一个外置正文被旧 checkpoint 引用；较新状态通过 Admit 把它放回 Resident。相同旧载荷，Simple 返回 1 个根，服务适配器返回 0。真实服务进程的 reconcile 报 `deleted_stale=1`；再恢复旧 checkpoint，正文已不可读。删除的是探针临时目录中的测试正文。

**最小修复。** B 线沿既有进程协议透传保护根和完整性，并为服务 checkpoint 提供真实且版本化的根解析；不能解析时明确“不完整/不支持”，使删除延期。不要把所有正文扫描成根，也不要在 Runtime 另存一份猜测的 Context 格式。必需回归：同一保留载荷在 in-process/service 两种入口保护结果一致，Admit→reconcile→恢复旧 checkpoint 后正文仍可读。

## R3-02 / P1：退休 scope 被旧 blob 带回，下一检查点无法恢复

**触发与源码。** scope close/外置会主动将 entry 的 `scope_id` 清为 None，随后退休无引用 scope（`scope.rs:466–477`；`gc/full/mod.rs:528–536`）。但 `store::reattach_owner_metadata` 仅在 entry 为 Some 或 blob 原本为 None 时覆盖 scope（`store.rs:1024–1028`），把“已释放”混同“旧格式未记录”。GC recall 合并后直接 push 到 heap（`gc/full/mod.rs:571–598`），带回磁盘中的已退休 scope id。

**实际反例。** 外置→关闭 Focus→GC 退休→通过 ResidentRequired 根召回，报告 `reactivated=1`，随后对引擎自己的 checkpoint 执行 restore，报 `references missing scope`。Admit 会重新绑定工作 scope，不能据其测试通过推断自动召回同样安全。

**最小修复。** A 线区分 legacy 缺字段与当前 owner 主动释放的语义，当前 owner 的已释放事实必须胜出；迁移旧数据时也不能生成悬空引用。复用现有合并点与结构校验，增加外置→退休→显式根召回→checkpoint/restore 的真实链回归。

## R3-03 / P1：更细的词语启发式仍能永久撤销兼容要求

**触发与源码。** `names_replaced_object` 接受替换宾语与旧要求的任意一个内容词相同（`gc/reachability.rs:272–275`）；`queue_decision_supersessions` 据此进入不可逆 `Superseded`。前序已增加停用词、否定词、宾语短语和引用片段等判断，仍没有证明被撤销的是哪条要求。

**实际反例。** 同一任务先输入 `use AuthService.rs with a 5-second timeout`，随后输入 `replace timeout logging in AuthService.rs with structured events`。真实 ingest/maintain 后，旧五秒超时记录为 `Superseded`。修改超时发生时的日志方式没有撤销超时时长。本项是 CTX-2 的新残余反例，不重开已修的旧句式。

**最小修复。** A 线收窄永久撤销的证明条件，要求能指向具体旧 decision identity/明确撤回依据；不确定则保持 Live。实体、词语和相似短语可继续用于检索/相关性，不能单独授予语义终结权。不要继续堆中英文关键词或再造通用语义解析器。回归需同时覆盖兼容要求共存、明确撤回仍可用、四种 owner 的相同判定。

## R3-04 / P2：退休事实环保留最早记录，满员后丢掉所有新事实

**源码与反例。** 新 notes 在前，旧 notes 经 `extend` 放在后面，然后从前端裁剪（`scope.rs:791–796`）。执行 513 次真实 Tool scope 开闭并逐次 GC，环为 512 条，第一条仍在、最新一条消失。满员后新进入的 Task 完成事实也会被抛弃；`task_completion_recorded` 恰好只查询当前树与这个环（`scope.rs:804–812`）。

**影响。** 退休审阅不是最近窗口，且有界诊断环被当成完成任务自动召回的唯一事实来源。仅调换拼接顺序还不够：旧完成事实离开环后仍不能让保留正文自动取得“未完成”含义。

**最小修复。** A 线按时间维护事实环；将仍有正文需要的完成/禁止自动召回事实保存在既有权威记录中，或在事实不可用时保守处理，避免把有界 UI/诊断窗口升级为生命周期权威。不另建完成数据库。回归同时覆盖环溢出后的最新记录和完成任务正文的召回语义。

## R3-05 / P2：Pending 正文可 fetch，却不能参与 foreground 材料化

**源码与反例。** `plan_foreground` 依次找 Resident、Warm、Stored，漏掉 Pending（`materializer.rs:928–951`）。同一 `src/a.rs@rev-1` 的 Pending 正文，真实 `fetch_external` 成功，但 foreground 为 0 并报 Missing。前序 CTX-6 修复了 required/admit 等入口，没有覆盖这条使用当前文件的路径。

**影响与最小修复。** 执行者明明已有正文却要重复读取，store 故障期间更明显。A 线让 foreground 与 required/fetch 复用完整 owner 解析，保留各自的选择优先级、预算、版本与范围校验；不让 Pending 入选悄悄变成 Admit。验证四位置同一文件身份的投影一致性。

## R3-06 / P2：材料化的 Stored 读取绕过当前 owner 元数据合并

**源码与反例。** required Store plan 只携带 id/checksum（`materializer.rs:1429–1434`），读完直接使用旧 blob（同文件 `1489–1506`）；foreground Store 路径同类。既有 fetch/admit/GC recall 的合并辅助函数未被复用。探针设置当前 owner 为 `Pinned/Session` 后，fetch 返回当前值，但 required 材料化返回 `Working/Task`。

**影响与最小修复。** prompt 中的条目归属与保留属性和目录不同，后续预算、呈现与消费方不能依靠一致的类型化事实。A 线将当前 owner 快照与 checksum 一起带入有界读取计划，所有读回入口复用同一合并规则；内容/创建身份仍由 blob 负责。与 R3-05 同切片交付，测试验证最终 `MaterializedContext`，不能只验 merge helper。

## R3-07 / P2：不召回的候选消耗召回额度，相关正文长期饿死

**源码与反例。** Warm 扫描在 `reactivation_reason` 返回之前就执行 `remaining -= 1`（`gc/full/mod.rs:1055–1077`）。一个应命中的旧 Note，加一个更新但不可自动召回的 shell observation，预算设为 1，连续三次 GC 均召回 0，相关 Note 始终 Warm。默认预算 8 时对应八个无效候选，同一问题只是更晚出现。

**最小修复。** A 线只为成功的 heuristic reactivation 消耗该额度；扫描工作预算与召回数量若需分开，沿用已有游标/批次边界。保留 anchor 专用额度，不改打分与默认阈值。验证无效候选不阻塞后续 Warm/Stored 的有效召回。

## R3-08 / P2：CTX-8 背压接线仍未完成，总驻留可继续增长

**源码与反例。** Pending 满后 overflow 留在 Warm buffer 并报 flag（`gc/full/mod.rs:349–355`）；Runtime turn-final 路径只将报告写成事件并继续完成/接输入（`actor/turn.rs:2935–2951`），当前 Runtime 没有读取 `externalize_backpressure`。真实输入+持续不可写 store，pending cap=3 保持不变，但 6/20/40 轮后的 Warm 为 3/17/56，三次均已报背压。

**定性。** 这是第二轮明确留给 B 线的 CTX-8 后半，属于**已知未完接线**，不重复包装成新的 GC 算法缺陷。Pending cap 和批次写入已存在，不能据此称总体有界。

**最小修复。** B 线由 Actor 在现有边界消费报告，保存可查询的资源阻塞状态，限制继续产生正文，保留 status/cancel/修复 store 后继续的控制路径；已受理输入与工具副作用不得丢失。A 提供报告语义，契约与客户端投影由 B 单一合入。必要回归是故障跨轮→总 owner 数/字节稳定→存储恢复→按批排空→继续。

## R3-09 / P1：restore 没有隔离停靠的终局事务

**源码链，尚未运行门控反例。** `prepare_restore` 只调用 `ensure_no_active_turn`（`actor/restore.rs:15`）；后者只检查 turn（`actor/lifecycle.rs:149–156`）。显式完成的 `TerminalFreezePark` 可以没有 turn，却带有旧 `TaskTxn`、prepared Context 和终局任务快照（`actor/maintenance.rs:540–574`）。GC 完成分支在通用 stale 检查之前分派（`actor/tools.rs:862–871`），仅核对 gc_work 的 operation id（`actor/maintenance.rs:802–815`），所以 restore 推进 generation 没有让它失效。

**具体窗口。** 保存无任务 C→创建 T 并显式完成→停在 checkpoint maintenance→restore(C)→维护返回。恢复后的状态仍可能被旧事务继续 freeze/commit；失败续体也持有恢复前 Context 用于 rollback。`commit_task_completion_tail` 不可失败的任务提交与完成事件发布建立在“表没有变”的前提上（`actor/turn.rs:2661–2688`），而这个前提已可被 restore 打破。不能把未执行窗口的磁盘/内存后果写成实测。

**最小修复。** B 线在恢复安装任何状态前，拒绝或完整结算所有停靠的终局事务；旧事务不得对新恢复状态提交或回滚。沿用 Actor/Core 权威和现有操作道，不增加第二个事务管理器。门控回归比较恢复前后 Context、TaskManager、终局 checkpoint、事件和原完成回执。

## R3-10 / P2：并发 checkpoint 可覆盖唯一 gc_work 槽位

**源码链，尚未运行门控反例。** `ensure_idle` 仅拦 `is_commit_in_flight`（`lifecycle.rs:60–75`）；这个谓词只认 TerminalFreeze（`maintenance.rs:600–605`），普通 ReadOnlyCapture 不算占用。`spawn_gc_op` 在没有槽位准入检查的情况下直接赋值 `self.state.gc_work = Some(...)`（同文件 `737–741`）。

**具体窗口。** 空闲 capture A 等待维护时，capture B 仍能进入并覆盖 A；A 的 reply/JoinHandle 随槽位被丢弃。丢 JoinHandle 不会确认取消，A 的任务继续运行，回来的 operation id 又不拥有槽位而被忽略。后续 Stop 只追踪最后留下的工作。现有单请求停顿测试不能覆盖该窗口。

**最小修复。** 与 R3-09 同属 EXEC-10：所有边界 spawn 共用单槽准入、完成、恢复和停止规则；占用时返回明确 busy 或在既有机制里有界续接，不能覆盖。验证两个并发 capture 的每份回执都有确定结果，Stop 后无未结算任务。

## R3-11 / P2：冷完成查询结果有界，扫描与阻塞仍随完整日志增长

**源码链，规模/超大行回归未跑。** `read_trace_tail` 从文件开头 `reader.lines()` 到 EOF，逐行反序列化后才把结果裁成 ring（`crates/agent-storage/src/lib.rs:1443–1482`）。扫描运行在 journal 唯一 writer 命令循环（同文件 `1332–1337`）。Actor 在 `TaskCompletionLookup` 分支同步等冷查完成（`actor/commands.rs:398–407`）。

**影响。** 4096 条仅约束保留结果，不能约束 I/O、解码耗时或单行 String；长日志的查询可同时延迟日志 append/flush 和 status/cancel/stop。RPC timeout 只结束客户端等待，不能撤销已经进入 Actor 的整文件扫描。

**最小修复。** 继续 EXEC-8：在既有 JSONL 上做总扫描字节、单行大小有上限的尾部读取，超预算诚实返回不完整窗口；查询等待移出 Actor 命令分支，保持 journal 的写入与查询一致性。不要先增加索引数据库。验证历史增长下扫描上界、超大行 typed 错误和查询停顿期间控制响应。

## R3-12 / P2：provider 已知用量在失败结果中丢失

**源码核实，SSE 反例未跑。** Chat 输出 length、终止错误和缺 DONE 在读取 accumulator usage 前返回错误（`crates/provider-openai/src/lib.rs:641–663`）；Responses terminal error 在 finalize/usage 返回前退出（同文件 `800–825`）。已读取 usage 后的工具参数校验错误也只保留错误本身。`AgentError::reported_usage` 目前只识别 EmptyCompactionSummary（`crates/agent-contracts/src/error.rs:181–185`）。Runtime 主模型失败分支无条件发 Unknown（`actor/tools.rs:1599–1612`）。

**影响与边界。** 已知的失败尝试数字不能进入全成本账目，重试成功只带最后成功尝试的用量。当前 Unknown/下界标识是正确防御，本项不是宣称这些失败被假报为零；缺口在于已经收到的证据被降为未知，妨碍对账和优化。

**最小修复。** C 线让失败结果携带已收到的 usage，重试层保留逐次已知与未知部分；B 单一合入错误/OperationOutcome/事件的必要小契约，保持一次计账与 Main/Maintenance 区分。验证本地 SSE 的 length/incomplete＋usage、参数失败＋usage→重试成功，不需要真实付费调用。

## R3-13 / P2：失败退避依据全部候选，实际发送内容未变也再次调用

**源码与实际反例。** Rolling 的 `fold_request_digest` 哈希全部可折叠候选 ID（`context-baselines/src/rolling.rs:173–194`），而 `take_fold_job` 只取实际能进入 2000 字符输入的旧前缀（同文件 `287–311`）；维护在计划之前用前一个 digest 判退避（同文件 `620–628`）。最旧记录填满输入，第一次本地压缩器失败，再新增只改变候选尾部的记录；探针捕获到第二次调用，`same_source=true`、`same_folded_items=true`、两次 source 均为 2000 字符。

**最小修复。** COST-8 续片由 C 提出成本判定，A 单一修改 Rolling：提取不修改状态的有界 FoldPlan，退避 identity 与实际取料共用该计划，绑定实际 source 和相关请求字段。保留失败还源和 partial suffix 身份。新增未发送尾部不能解除退避；实际输入改变才解除。每 pass 调用 cap 已存在，不将本项描述为单次维护完全无上限。

## R3-14 / P2：GUI 的可丢弃渲染队列承担了不可丢的成本累计

**源码核实，dispatcher 回归未跑。** `_pendingUiEvents` 达 64 条后无差别 `RemoveAt(0)`（`apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs:990–997`），成本直到 UI drain 的 `HandleEvent` 才调用 `RecordModelUsage`/`RecordCompactionCost`（同文件 `1130–1137`）。刷新 snapshot 没有成本累计字段可补回（`crates/agent-platform-protocol/src/work.rs:470` 起的 WorkSnapshotResponse）。

**具体窗口。** UI 暂停期间，一条 ModelUsed 后跟 64 条其他事件，成本行可在累计前被淘汰；恢复 UI 后数字少算，甚至继续显示没有模型调用。SDK durable 事件处理不会修复 GUI 自己的丢弃。

**最小修复。** C 线将固定大小的成本累计与 run/seq 水位放在丢弃渲染行之前，UI 只呈现累计结果；有断流缺口则标记不完整，不能把渲染 cap 拿掉换成无界队列。验证冻结 dispatcher、混合 Main/Maintenance、重连/重复 seq 与 render overflow。

## 保留的前序成果与后续限制

已经定向读到：热任务/完成回执共用 `bounded_hot_pair_window`；sealed locator 恢复引用与正文扫描分离；materialize/maintenance/full-GC 进入既有操作道；Pending required/终态/admit 以及三处 Stored merge；缓存 miss/write 分开及 Main/Maintenance 用量事实。这些实现不整体重做。新问题指向组合缺口，具体覆盖程度以本轮证据为准。

ExternalMap/Catalog 全量元数据与每次 checkpoint 的历史增长仍沿 CTX-9/N7 残余；真实 provider 同起点、同质量的全成本与长运行验收仍归 COST-5。没有付费模型调用，没有远端 CI，没有本轮完整 Rust/.NET 测试，也没有发布、提交或推送。

## 本轮执行与复用要求

- `cargo build --offline -p agent-context-service --bin agent-context-service`：成功；确保进程探针对应当前源码。
- `cargo run --offline --manifest-path docs/reviews/2026-09-13-executor-maintainability-audit/evidence/context-probe/Cargo.toml --target-dir target --quiet`：成功完成九个反例，结果见 JSONL。此处成功是观测完成，不是修复验收。
- `cargo fmt --manifest-path docs/reviews/2026-09-13-executor-maintainability-audit/evidence/context-probe/Cargo.toml`：只格式化本轮隔离探针。
- 全仓目录/词法扫描与源码 SHA-256 对照用于说明范围、确认所读源码；不将匹配数量或文件大小直接当作缺陷。
- `python scripts/doc_consistency.py`：`document-consistency gate: OK (13 live docs, links and state agree)`；四份当前文档的 `git diff --check` 通过，新审查包相对链接检查无缺失。
- 起止 507 个基线文件 SHA-256 对照：**0 个产品/基线文件漂移**，见 [source-drift.json](evidence/source-drift.json)。本轮新文件为审查包/隔离探针，另对 CURRENT/NEXT_TASKS/AUDIT_TODO/ROADMAP 添加当前入口；既有产品修改保留。
- 功能实施时只运行对应回归，集成沿用现有 CI，不额外开启反复全仓验收循环。
