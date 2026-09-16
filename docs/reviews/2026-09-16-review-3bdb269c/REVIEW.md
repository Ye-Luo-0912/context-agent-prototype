# 后端长流程与 TUI 续审 — 3bdb269c

## 基线、证据与边界

- 仓库：`Ye-Luo-0912/context-agent-prototype`。
- 固定 SHA：`3bdb269cba4e1bdd0fe2e71007dfa1ad7640b28d`；收尾再次读取 main，仍为该 SHA。
- 提交时间：2026-09-16 14:17:19 UTC / 2026-09-16 23:17:19 Asia/Tokyo。
- 该 SHA 的 CI：run `35107501790`，attempt 1，completed / success。不要与父提交 `3d0b114f` 的 attempt-2 回执混淆。
- 方式：连接器固定版本源码审查、提交差异核对、生产者—消费者交叉检查、上游库文档/源码核对。
- **未完成全仓所有文件逐行覆盖，也未执行本地 Rust/.NET 回归、实际 PTY 或真实供应商实验。** 全仓范围不等于每一行都已经核验；具体正文范围见 COVERAGE.md。
- 本轮实际探测未找到 cargo/rustc/dotnet，git 远端访问 DNS 失败；源码来自可用的 GitHub 连接器。额外的源代码归档下载也未取得可用文件。这里的条件反例不是已运行成功的仓库测试。
- 没有修改、提交或推送仓库。本报告是审查与执行建议，不是已应用补丁。

## 总体判断

本批变化主要在 TUI 和共享 StatusProjection。已经增加完整审批资料、按换行拆分正文、终端 guard、命令 worker、结果卡任务身份与遗漏数量、headless 事件缺口处理。这些进展保留，不原样重开旧工单。

新问题集中在三类边界：

1. **身份的作用域不同**：日志序号不等于流式分片序号；单张结果卡的修订号不等于全局快照提交序号。
2. **已知成本不等于业务终态**：旧操作结果失效，已知用量仍应结算；用量到达也不表示当前操作结束。
3. **布局、日志窗口和队列的“有限”不是同一个保证**：可滚动不等于尾部可达；保留 400 条消息不等于附属身份集合有界；有序命令队列不等于普通输入也有序。

建议先修 R1、R2、R4/R5；R3/R7 与共享读模型一起收口，R6 继续原 U6，R8 继续原 U1/U2 的真实渲染验收。旧 B1/B2 沿原工单推进。不要建立新架构阶段或另一套运行权威。

## R1 — 实时分片被当成重复的持久事件丢弃

**优先处理；静态控制流确认。**

位置：`agent-runtime/src/sink.rs::LiveSink`、`actor/model.rs` 创建 LiveSink 的位置、`agent-tui/src/state.rs::{claim_event,apply_runtime_event}`。

生产者先发布持久 ModelStarted，然后取 `core.event_sequence()` 作为 LiveSink 的 journal cursor。LiveSink 的 TextDelta 和 Retrying 都复用这个 cursor；它们不写 WAL，不申请新的持久 seq。

消费者现在在任何事件进入 reducer 之前，以 `(RunId, seq)` 去重。ModelStarted 已经占用该键，随后正常流式分片和重试提示都在最外层返回；操作代际检查没有机会执行。

```text
同一个 RunId：
ModelStarted(seq=100, op=A)
ModelDelta(seq=100, op=A, "第一段")
ModelDelta(seq=100, op=A, "第二段")
ModelRetrying(seq=100, op=A)
AssistantMessage(seq>100, 完整正文)
```

影响：常规交互无法实时显示输出和重试进度。最终持久 AssistantMessage 仍可能显示，因此不是全部回答丢失、模型停止或正文存储损坏。

**最小修复**：持久事件使用日志身份去重；实时进度使用 `(TurnId, OperationId, generation)` 校验归属。只有确需对实时分片去重时，才增加单流分片身份，不能挪用或伪造持久 WAL 序号。历史重放水位也不能直接过滤当前流的实时分片。

**验证**：使用真实 LiveSink/Runtime 生产事件，固定 RunId，两个分片和重试共享 ModelStarted 的 seq，证明实时文本到达。旧 generation 的分片继续被拒绝，重复持久 ModelUsed/TurnCompleted 不重复计数。

**测试维护性**：现有 `late_deltas_from_a_superseded_operation_are_dropped` 的 `envelope()` 每次调用生成不同 RunId。这可以绕开新的 `(RunId,seq)` 去重，却不代表正常单运行流。应修正 fixture 的身份关系，不是削弱断言。

证据：E1、E2、E3、E4。

## R2 — 取消后的补账仍漏掉迟到的成功/失败结果

**P2；新发现的 W4 终态矩阵残余。**

位置：`actor/turn.rs::{emit_cancelled_usage_row,usage_already_accounted}`、`actor/tools.rs::on_operation_completed_inner`。

取消屏障可能先记录 Unknown 用量，并把 OperationId 放入 `usage_accounted_ops`。迟到结果命中 stale 分支后，已有记录的补充路径只接纳 `OperationOutcome::Cancelled { known_usage: Some(...) }`。迟到的 ModelOutput 和 Failed{usage} 不走这个补充；非 accounted 的 stale 分支还只有 ModelOutput/维护报告等特定路径。

```text
调用 A 已实际执行
→ 用户取消先被 actor 处理，写 Unknown 占位并标记 accounted
→ A 返回成功 ModelOutput（或者 Failed 携带已报告 usage）
→ A 的业务结果因代际失效被拒绝，这是正确的
→ 其 usage 因已 accounted 且不属于 Cancelled 变体也被拒绝，这是缺口
```

模型任务并非全部通过 abort 强制结束；即使供应商合作取消，响应完成/完成消息入队与 actor 处理取消也存在合法竞态。这里不需要真实供应商忽略取消才成立。

**最小修复**：把使用量提取从业务结果分支中收敛出来，覆盖 ModelOutput、Failed、Cancelled；将“已有 Unknown 占位”和“已结算全部可见证据”分开。沿现有 operation/accounting 状态实现一次性补账，不另外建设计费平台；如未来允许多份增量证据，需要明确累计值/增量值而非直接相加。

**验证**：三个迟到终态分别测试；关闭可选 metrics 文件；业务始终保持已取消且不执行返回工具；已知 input/output/cache read/cache write 进入正式账目一次；重复 completion 不重复补账。

证据：E5、E6。原有“backoff 中取消带 known_usage”的测试继续保留，不按旧描述重做。

## R3 — 旧调用的用量会清掉新调用的运行状态

**P2；共享 StatusProjection 的语义耦合。**

`StatusProjection::fold(ModelUsed)` 无条件将 `in_flight=None`。但 Runtime 已明确支持旧/失效操作的迟到费用补充。

```text
A 取消 → B 的 ModelStarted/ToolStarted → A 的迟到 ModelUsed
```

在该顺序下，B 仍运行，共享读模型却显示没有 in-flight。这里只是投影不准确，不能说 Runtime 真的终止了 B。

**最小修复**：用量事实只负责账目；活动状态由能绑定当前操作的生命周期事实推进。需要从用量关联终态时，携带明确 OperationId/角色/代际并核对。旧格式没有身份的用量记录只计费、不清理当前操作。

**验证**：A/B 交错，A 的补账一次入账但不改变 B 的运行显示；主调用与维护调用分开；实时消费、日志回放结果一致。

证据：E5、E7。不要通过丢掉迟到 ModelUsed 来“修好界面”。

## R4 — 结果卡的任务级修订号被当成全局写入水位

**P2；确定性的跨任务持久化问题。**

`begin_card_for_task` 切换任务时使用 default/mem::take，使新卡的 revision 从 0 开始。`persist_result_card` 对卡自己的 revision 加一。但 `card_snapshot_gate.last_written_revision` 是跨任务共用的全局水位。

```text
任务 A 完成：revision=1，快照写入，全局 last_written=1
切换任务 B：新 ResultCard.revision=0
任务 B 完成：revision=1
writer 判断 1<=1，拒绝写入
```

进程内的当前 B 卡可能正确；重启后读取的 latest 快照仍是 A。Runtime 的正式 TaskCompleted/任务记录不受此视图缓存问题影响。

**最小修复**：快照发布序列在 AppState/发布器层单调推进，不能随任务切换或投影重建归零；卡片自身版本与全局发布顺序分开。保留单写者和原子提交。重放过程中不要把历史 TaskCompleted 逐个当成新发布动作。

**验证**：同一个 AppState、真实临时状态目录、连续完成 A/B/C；异步写入反向调度；重启读回的最新任务仍为 C；重复重放不能发布旧任务覆盖新快照。

证据：E8、E3。

## R5 — 遗漏检查的结果没有统计，却显示成全局失败数量

**P2；U4 的数值口径残余。**

`ToolFinished` 在 checks 满额后只增加 omitted_checks。`failed_checks()` 仅统计已保存的 checks。`format_result_lines` 却将该值与 total_checks 并列显示为 recorded/FAILED。

最小反例：32 个成功检查后再来 1 个失败检查，结果变成 `33 recorded, 0 FAILED, 1 not shown`。虽然已经提示有遗漏，但明确的失败数量仍不正确。

**最小修复**：总成功/失败数在显示裁剪前、按事件身份结算；显示列表只是摘要窗口。或者明确将失败数量限定为“已展示记录”，对未展示的结果不声称 0。失败信息可优先保留，但不能修改真实执行结果。

**验证**：第 33 项失败；连续多项遗漏失败；重复事件；任务切换。容量保护与事实完整性同时成立。

证据：E9、E10。

## R6 — 有序 worker 没有包含普通用户输入

**P2；继续原 U6，不重复设计调度器。**

`SessionCommand` 覆盖 Focus/Activate/Restore/Continue 等；普通非命令文本仍通过独立 `tokio::spawn(handle.user_message(...))` 直接发送。Actor 只串行处理已经到达的命令，不能恢复客户端未保持的键入顺序。

可控反例：暂停命令 worker，队列中已有 `/task B` 或 `/restore ...`，再提交普通纠正文本。普通文本越过 worker 到达当前 A/旧状态。

**最小修复**：具有用户语义顺序的普通文本和任务切换共享有界提交通道；使用既有 expected_task_id/steering 入口表达目标。紧急取消可独立，但必须定义它如何处理尚未提交的队列。不要简单把所有操作放入一个阻塞队列，导致取消也排在慢 I/O 后面。

worker 的 JoinHandle 也应由 session 持有，退出时停止接单并明确取消/结算尚未提交请求；当前 detached worker 的生命周期是维护性残余。本文不据此宣称 Core stop 后仍能越权执行。

**验证**：真实键入顺序 `/task B`→普通文本、`/restore`→普通文本、取消与排队输入、退出时队列非空；每个输入有明确的提交或拒绝回执。

证据：E11。

## R7 — 统一 reducer 之后，对话重建仍不是幂等且附属集合无界

**P2，可随 R1 的读模型切片收口。**

重放会保留现有 messages、shown_message_index 和 shown_input_ids，然后清理 applied 身份再走 apply_event。User/Assistant/Tool 行有专门的消息身份去重，但 Warning/Focus/ModelUsed 等生成的 SYSTEM 行没有同等去重，会重新 append。

例：当前 400 条可见消息由 1 条 Assistant 回复和 399 条 warning 组成。重放同一日志时，Assistant 因已展示被跳过，warning 再次追加；重复的 SYSTEM 行反而把原先保留的 Assistant 行挤出窗口。这里不是持久日志被删，而是相同日志重建得到的可见对话不一致。

另一个独立资源事实：`shown_input_ids` 仅插入 HashSet，没有与 MAX_RENDERED_MESSAGES/MAX_SHOWN_MESSAGE_EVENTS 对应的淘汰规则；保留 400 条消息并不保证这张附属表有界。

**最小修复**：所有事件派生行共享事件身份；更稳妥的是重建一份有界 read model 后原子替换，只保留草稿、滚动、当前审批等真正本地状态。reducer 输出快照/展示动作，与写盘等副作用分开。InputId 索引跟随可见窗口和活动排队输入，而不是无限保留全运行历史。

**验证**：同一日志重放两次可见序列一致；相同正文的不同事件都保留；日志重放不重写历史卡片；固定窗口下数万次不同输入不使辅助集合线性增长。不得以清空输入框或当前审批来简化恢复。

证据：E3、E8、E12。

## R8 — 审批滚动仍用宽度除法，自制 Unicode 宽度也不等价

**P2，原 U1/U2 的渲染口径残余；需要实际 TestBackend 回归。**

`wrapped_rows` 和 conversation 的 row count 仍使用 `display_width(line).div_ceil(width)`。实际 Paragraph 使用按词边界的折行；空余列会使实际行数多于简单商。审批 scroll 被错误的 max_skip 截断后，反复 PageDown 仍可能到不了真实尾部。

现在的手写 `char_display_width` 还把 combining mark / zero-width joiner 等算成 1；这不是 Ratatui/UnicodeWidthStr 的真实规则。例如 `e + U+0301` 不能按两个显示列计算。现有 comment 将手写规则称作等价，需同步修正。

**最小修复**：复用所用 Ratatui 版本的宽度/布局语义。可评估该版本受特性门控的 Paragraph::line_count，或先用同一布局生成可见行、渲染时不再二次 Wrap。不要为减少修改文件数维护第二份 Unicode 表，也不需要引入全功能编辑器。

**验证**：窄宽度、许多不能拼在同一行的单词、组合字符/ZWJ、中文、缩进和尾部 sentinel；必须调用真实 ui::render/TestBackend，不只比较字符串或宽度辅助函数。

证据：E13；上游 WordWrapper 与 unicode-width 文档。上游当前源码用于核对规则；最终修复仍须按仓库 Cargo.lock 对应版本运行，不把外部源码阅读当作仓库实测。

## 保留的后端工单，不重新编号

比较 d92564bc 与本轮 SHA，生产变化集中在 TUI 与 StatusProjection，context-simple 未在该区间修改。最新 NEXT_TASKS 也把 B1/B2 保持为后续动作。

- **原 B1**：一批 required 冷目标逐个安装后相互降级；改为解析产生有界、版本/范围绑定的规划材料，而不是依赖最后的热表。
- **原 B2**：首次采用已存在 card 文件时不能仅凭 exists 就移除 checkpoint 的可靠 inline 元数据；验证失败时保持 inline 或原子修复。
- **原 B3**：ledger 导出先 take 后 await 的取消安全性，可在前两项之后完成；不是 authority WAL 问题。

文档记载其他工作树有未提交改动，但本轮无法观察用户本地工作树；不能把该记载当作修复已经进入 main。先按所有权接续，不覆盖在飞改动。

## 供应商 KV 与下一阶段

本轮不重复打开已修的普通内容块/工具结果块映射。下一步的成本对照首先需要 R2 的完整结算；否则只比较最终成功调用，会漏掉失败/取消成本。

实际序列至少包含：稳定策略与有效证据、只改变当前焦点、新读取、代码版本改变、checkpoint 重建、维护调用、取消以及迟到补账。记录普通输入、缓存读、缓存写、输出和未知覆盖，并按实际端点口径正规化。

缓存 key 是路由，不是内容正确性的证明；断点可发送不等于供应商已接受、实际命中或净费用下降。文件/权限/任务约束真正变化时必须失效，不能为命中保留错误上下文。真实供应商实验继续按 T8 条件执行，未运行则为 NOT_RUN，不报告降本比例。

## 可维护性建议

1. 事件身份显式区分 durable / live；测试固定正确的 RunId/seq/OperationId 关系。
2. 模型 operation 的业务状态与 usage settlement 正交；取消不是删除已知成本的理由，成本也不是清空当前状态的指令。
3. 任务卡内容版本与视图发布序号正交；显示窗口与全量计数正交。
4. 用户输入顺序在前端提交处保留；慢存储与控制通道分离；不增加第二个执行器。
5. 依赖库已有的 Unicode/折行规则应复用；按接口责任决定修改边界，不以“只改三文件”牺牲正确性。
6. 活跃文档只保留开放动作和原回执链接。不要再次把本报告全文追加到 CURRENT/NEXT_TASKS。

## 未升级为新阻塞的检查

已扩展阅读 RollingSummaryEngine 的完整生产实现、run_summary 聚合以及若干完成/证明处理链。它们不因为文件大就自动成为缺陷；本轮没有运行其并发/性能回归，不作性能数字或生产故障保证。

离线 user_messages 等聚合若要作为“独立用户输入数”，应明确 queued/applied 生命周期的统计口径；这里未将报告字段用途的推断升级为新的执行正确性缺陷。

## 证据索引（全部项目文件固定同一 SHA）

E1 `crates/agent-runtime/src/actor/model.rs`，1600–1815：发布 ModelStarted、创建 LiveSink、生产 provider 调用、结果分类。
E2 `crates/agent-runtime/src/sink.rs` 全文：实时事件复用 journal_cursor。
E3 `crates/agent-tui/src/state.rs`，650–950：replay、claim_event、reset、task card 切换。
E4 同文件 1610–1950：format_result_lines、流式测试的 envelope helper。
E5 `crates/agent-runtime/src/actor/tools.rs`，710–1030：stale 结果与已结算用量补充。
E6 `crates/agent-runtime/src/actor/turn.rs`，3300–3645：取消占位、accounted/supplemented 队列、取消动作。
E7 `crates/agent-runtime/src/status.rs`，1–290：ModelUsed 与 in_flight。
E8 `crates/agent-tui/src/state.rs`，350–650：附属索引、card snapshot、replay 初始化。
E9 同文件 1–235：ResultCard 与 failed_checks。
E10 同文件 1300–1535：ToolFinished 的显示裁剪、TaskCompleted。
E11 `crates/agent-tui/src/session.rs`，1–640 与 830–1030：命令 worker 与普通输入分流。
E12 `crates/agent-tui/src/state.rs`，780–1120/1260–1610：统一 reducer、消息去重和 SYSTEM 输出。
E13 `crates/agent-tui/src/ui.rs`，1–320：scroll、宽度和渲染。
E14 `crates/agent-replay/src/run_summary.rs`，1–370：离线聚合。
E15 `crates/context-baselines/src/rolling.rs` 全文：折叠、取消守卫、材料化、checkpoint/restore。
E16 `docs/NEXT_TASKS.md` 当前入口/接手顺序：A 关闭状态、B1/B2 待接续、历史内容仍在默认入口。

源码基址（将相对路径接在后面）：
`https://github.com/Ye-Luo-0912/context-agent-prototype/blob/3bdb269cba4e1bdd0fe2e71007dfa1ad7640b28d/`

外部核对资料：
- `https://docs.rs/unicode-width/latest/unicode_width/` — combining/ZWJ 与字符串显示宽度。
- `https://docs.rs/ratatui-widgets/latest/ratatui_widgets/paragraph/struct.Paragraph.html` — scroll 和受特性门控的 line_count。
- `https://github.com/ratatui/ratatui/blob/main/ratatui-widgets/src/reflow.rs` — WordWrapper；本轮读取 blob `21072925405d6ed00f0fc973ca254de128423374`。
- `https://developers.openai.com/api/docs/guides/prompt-caching` — 精确前缀、路由与写入策略；真实端点仍需独立验收。
