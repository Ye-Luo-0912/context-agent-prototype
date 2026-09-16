# 当前事实与工作范围

本页是当前状态入口。执行任务及状态只维护在 [NEXT_TASKS.md](NEXT_TASKS.md)。旧报告中的"当前"只属于其固定基线。

## 已核对基线

- **续审基线 `3bdb269c`（2026-09-16，U 批次之后）**：CI run `35107501790`，**attempt 1 success**（勿与父提交 `3d0b114f` 的 attempt-2 回执混淆）。该次续审发现 **U3 引入的回归**：`claim_event(RunId, seq)` 把 `LiveSink` 复用 `ModelStarted` 游标的实时分片（`ModelDelta`／`ModelRetrying`）当成重复持久事件丢弃 → 正常流式显示与重试进度被破坏（**R1，最高优先**）。另开 R2–R8（费用补账终态矩阵、用量误清 in-flight、卡片修订号与全局发布序号混用、遗漏检查的失败计数、普通输入未进有序通道、重放非幂等与附属索引无界、审批滚动仍用宽度除法＋自制 Unicode 表）。报告与范围见 [docs/reviews/2026-09-16-review-3bdb269c/](reviews/2026-09-16-review-3bdb269c/REVIEW.md)；行动见同目录 [NEXT_ACTIONS.md](reviews/2026-09-16-review-3bdb269c/NEXT_ACTIONS.md)。**A 线不能视为全部关闭。** R1–R5 已修复并提交：R1（`bb761547`）real-time 分片不再按日志游标去重（**契约里早有 `RuntimeEvent::is_live_only()` 且 agent-host 已在用**，TUI 未采用；根因之二是折叠 fixture 每次新建 RunId 却固定 seq=1，恰好绕过该身份）；R2＋R3（`8877a4da`）迟到 `ModelOutput`/`Failed` 的已知用量一并结算、用量事实不再清当前操作状态；R4＋R5（`d30f2956`）快照发布序与会话单调而非随任务归零、失败计数在显示裁剪前结算。回执：[R1–R5](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。**R6／R7／R8 与 B3 亦已关闭**（`a0f23d72`／`ef8a835c`／`3fe396c9`）：普通输入进入同一保序通道且 session 持有 worker；重放先丢弃事件派生行再重建（同一日志两次渲染一致）；滚动上限与绘制共用 ratatui 的 `Paragraph::line_count`、宽度改用 `unicode-width`；ledger 导出改为「快照→提交→确认消费」，取消不再丢行。回执：[R6/R7/R8/B3](reviews/2026-09-16-review-3bdb269c/R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md)。**仍开放：B1（批量 required 有界解析计划）与 B2（existing card 认领校验）**；`context-simple` 的在飞工作已按归属先提交（`ca6c7254`、`99218532`），基线干净。**B1／B2 亦已关闭**（2026-09-17，`de6bf061`／`2482f3ef`）：解析时捕获版本/范围绑定的卡片条目作为有界计划源（exact ID／实体／前景三处 fallback），批内驱逐不再把已读到的必需正文压成 `Missing`（真实预算不足报 `BudgetExcluded`）；capture 对已存在的卡片路径只在读回字节与计划一致时认领，可读不一致走同一原子写入修复、写失败保持 inline，坏引用不再进入 manifest。回执：[B1/B2](reviews/2026-09-16-review-3bdb269c/B1_B2_THIRD_BATCH_RECEIPT.md)。**至此 R 系列与 B1/B2/B3 全部关闭**，剩余 T8 条件任务与 C（续）实际请求序列的 KV 成本比较。
- 阶段审查基线：`4aaa8bea89336e2ec0fd21c76d04967814b24020`（2026-09-14）。该 SHA 的 CI run `34788369834` 最终 success，**第 2 次尝试**（满载抖动重跑），非首次全绿。审查报告：[下一阶段审查](reviews/2026-09-14-next-stage-review-4aaa8bea/REVIEW.md)。
- main 之上另有并行分支在飞（如 `codex/headless-output-budget`：真实 DeepSeek 任务记录 headless 输出缺口、失败轮结算与输出预算修复）。采用任何结论前核对实际分支与 HEAD。

**2026-09-15 续审（基线 `258eb4eb`）发现并已修复 S1**：T4 二期的降级回归测试曾持 state 锁调用 fetch_external（内部重取同一锁），确定性自锁导致 CI run `34917761534` 两 job 取消——已修（锁分段＋集合断言），[续审报告](reviews/2026-09-15-continuation-review-258eb4eb/REVIEW.md)。续审同时确认：T1 统一装箱/T2 增量目录/T3 有界发现/T6 键编码已有实现保留不重开；新开 S2a（装箱身份/覆盖一致性）、S2b（官方缓存 mapper 形状修正——本地可完成不等付费实验）、S3（固定预算冷目录闭环）、S4（T7 独立进程＋指令传递证据）。

**2026-09-15 进展**：第一批 T1/T2/T3 与第二批 T4/T6/T5 已全部合入 main（T1 统一装箱＋诚实降级；T2 增量目录维护；T3 有界进程与发现；T4 冷目录预算化第一期；T5 一份有效配置；T6 键编码迁移＋断点形状 fixture）。各片本地全量绿＋clippy 0，远端 CI 以 run 记录为准。

**2026-09-15 续审批次收口**：S1/S2a/S2b/S3/S4 全部关闭（见 [NEXT_TASKS.md](NEXT_TASKS.md) 对应行与回执）。S3 固定预算冷目录闭环（per-read deadline、字节预留、保 claim 访问戳、类型化背压、搜索 coverage+continuation 进模型正文）；S4 独立进程变体＋指令传递证据（真实两进程、脚本 provider 内容门禁）。同批修复三处满载 Windows 抖动：t7 旅程审批交付后的立即断言（`4ba60b97`）、cold_bounds 捕获 io 墙钟预算 2s 过窄（随 S3 关闭）、host_e2e stop 唤醒在死管道上盲转 panic（`48ddcb1f`）。残余限制如实记录在两份回执（wire 层 coverage 传播、续查 token 不进 checkpoint、pending 目录随历史增长）。T8 仍为条件任务（付费实验），无新开任务。

**2026-09-16 新审查（基线 `4f6eb7ff`，CI run `35019429861` 首次成功）**：开 V1–V7 七项发现，核心主线是"分页只改变驻留位置、不改变语义身份与保护义务；取消只改变执行结果、不抹掉已知成本"。报告与覆盖表见 [docs/reviews/2026-09-16-review-4f6eb7ff/](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)。

**2026-09-16 新审查（基线 `d92564bc`，CI run `35036238204` 首次成功）**：开 U1–U7 与 B1–B3 十项发现。主线是"TUI 已经不是显示壳——它的审批、命令顺序、事件恢复和任务复核直接影响长流程可控性，应作为后端主体的一部分收口"。首次全文读取 `agent-tui/src` 八个源文件（含内联测试）、`tests/real_binary_startup.rs` 与该 crate 配置。报告与覆盖表见 [docs/reviews/2026-09-16-review-d92564bc/](reviews/2026-09-16-review-d92564bc/REVIEW.md)；派工与停止条件见同目录 [NEXT_ACTIONS.md](reviews/2026-09-16-review-d92564bc/NEXT_ACTIONS.md)。架构不重做，三线不变，GUI 继续后置。

**2026-09-16 U 批次收口（A 线）→ 已被续审部分推翻**：A1／A2／A3／A4 的**实现与提交**均已完成（`22ab97a6`／`baa2ca70`＋`7224ec6d`／`296ec005`＋`96c0e5c3`／`296ec005`），但 `3bdb269c` 续审确认 **U3 的精确一次身份去重引入回归（R1）**：`claim_event(RunId, seq)` 会把 `LiveSink` 复用 `ModelStarted` 游标的 `ModelDelta`／`ModelRetrying` 判为重复并整体丢弃。**因此 A 线不能视为全部关闭**，R1–R8 见第六批。已完成的改进（完整审批详情、多行拆分、终端 guard、有序命令 worker、任务卡隔离、headless 事件缺口）保留，不按旧问题重做。

**2026-09-16 U 批次进展（第二批）**：A2（U3＋U4）关闭，A 线只剩 U6。U3（`baa2ca70`）：`apply_runtime_event` 先 claim `(RunId, seq)`，被拒即整体返回（修复前只跳过投影折叠、本地字段仍被改），折叠体抽到 `apply_event` 供**实时与重放共用**，`resync_projection` 先归零事件派生字段再重建，转录行改按**事件身份**去重（5 处正文比对删除，相同文字不同 turn 都保留），`AssistantMessage` 只终结本轮流式打开的行，`StatusProjection` 补折叠 `TurnCancelled`，坏行/短读/序列缺口 → `view_partial` 且不设连续水位。U4（`7224ec6d`）：`ResultCard` 按 `task_id` 归属、切换任务归档旧卡，容量拒绝改为**计数**并在 review 明示遗漏，快照单写者＋版本化＋rename 原子提交。回执：[A2](reviews/2026-09-16-review-d92564bc/A2_U3_U4_EVENT_MODEL_AND_REVIEW_RECEIPT.md)。**仍未做**：U6（`session.rs` 20 处 detached spawn 不保序、慢 I/O 占住绘制循环）、B1/B2（`context-simple` 当前有他人在未提交改动）。新增回归以变异恢复法复验 6 处转红；agent-tui 83/0、real_binary_startup 2/0、agent-runtime `status::` 5/5、clippy 0、fmt clean；未跑 agent-runtime 全量与 workspace 全量 CI。

**2026-09-16 U 批次进展（第一批）**：A1（U1＋U2）与 A4（U7）已落地，A3 只剩 U6。A1（`22ab97a6`）：`PendingApproval` 保留完整请求（无 220 字符上限）＋可滚动审批详情＋`PgUp/PgDn`＋确认绑定屏上 `request_id`（过期确认不批准新请求），对话摘要截断标 `…`；`conversation_lines` 按 `'\n'` 拆真 `Line`，折行/滚动/光标共用 `display_width` 显示列宽。A3（U5，`296ec005`）：`TerminalGuard` 跟踪并逆序恢复已启用终端状态（早退回滚部分状态、`Drop`＋panic hook 链回原 hook），终端释放排在 `composed.shutdown()` 之前且两类错误分别聚合。A4（U7，同上）：`Lagged` 缺口与新 `EXIT_INCOMPLETE = 4`（`events_dropped`｜`stream_closed`），尾部 `TurnCompleted` 不再掩盖被丢段，另修 JSONL 双换行。回执：[A1](reviews/2026-09-16-review-d92564bc/A1_U1_U2_APPROVAL_AND_RENDERING_RECEIPT.md)、[A3/A4](reviews/2026-09-16-review-d92564bc/A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT.md)。**仍未做**：A2（U3 共享事件读模型＋U4 按任务 review）、U6（`session.rs` 20 处 detached spawn 不保序、慢 I/O 占住绘制循环）、B1/B2（`context-simple` 当前有他人在未提交改动）。本批新增回归均以变异恢复法在 `d92564bc` 上复验转红，agent-tui 75/0、clippy 0、fmt clean；未跑 workspace 全量 CI。

**2026-09-16 V 批次收口**：W1–W4 全部关闭（见 [NEXT_TASKS.md](NEXT_TASKS.md) 第四批与各回执）。W1（`c0923af2`）：scope 退休引用闭包许可（未读冷页引用的 scope 不被退休、预算耗尽诚实推迟）＋必需正文冷解析（typed Missing/Corrupt/IoFailed/UnreadColdPage，不再把已存在正文报成 Missing）；W2（同上）：`ContextSearchResult` 原子返回＋wire 协商、fresh/resume 生命周期、restore 失效旧 token＋nonce 防ABA；W3（`9b176df0`）：工具结果断点改 `input_text` 块、未确认 sibling fallback 删除；W4（`4fa2d8a2`）：取消结算已知用量（`Cancelled{known_usage}`，observer 降级为诊断副本，全链回归在无 metrics env 下验证）。限制如实记录在各回执（退休探测预算耗尽时持续推迟、covered 集仍为累积 ID 集、maintenance lane 取消仍 unknown、真实端点接受/命中归 T8）。

**2026-09-17 新审查（基线 `6afa25df`，即 B1/B2 收口后的 docs 提交）**：开 Q1–Q6 六项发现与 O1–O3 非阻塞观察。主线：**正文"准备好了"还要能提交消费；连接"还活着"还要能继续交付事件；业务结果"失败了"也不能丢掉已知费用。** 核心是 Q1——B1 捕获让冷必需正文进入最终请求，但 `acknowledge_consumption`/`has_exactly_one_owner` 与 `access::stamp` 只认可四种已加载 owner：ACK 拒绝有效消费，且该失败发生在 `ModelUsed` 发布之前，已知用量一并丢失（Q2）；Provider 流内多种提前退出绕过 accumulator 用量结算（Q3）。报告、覆盖表与实施任务见 [docs/reviews/2026-09-16-review-6afa25df/](reviews/2026-09-16-review-6afa25df/REVIEW.md)。该 SHA 的 CI run `35134020808` 审查读取时 attempt 1 in_progress，不借用父提交绿色结果。四个实施切片（QA/QB/QC/QD，文件所有权互不重叠、可并行）见 [NEXT_TASKS.md](NEXT_TASKS.md) 第七批。

## 当前阶段：可持续使用的后端开发流程

目标：**同一 Agent 在同一任务与工作区内，持续完成计划、检索、修改、验证、中途纠正、中断、冷恢复和交付；热资源、维护工作和供应商缓存成本有明确边界，核心规则在少数实现入口维护。**

不是"继续关闭审查项"，也不是全仓重写。每推进一个主体功能，同步消除该功能涉及的重复决策、隐式约定和状态分歧（可维护性是切片验收条件）。GUI 维持必要兼容，不扩展功能。

三线不变：**A 执行核心与工具；B Context/GC/搜索；C 平台/供应商 KV 与成本。** 引用历史问题时带报告日期与原始编号。

## 上一阶段成果（已关闭，回执可查）

- 文档入口已分离职责；文档检查只验证机械结构。[应用回执](reviews/2026-09-14-docs-entry-review-2b43186b/APPLICATION_RECEIPT.md)
- B 线恢复数据保全（N01–N03）＋ hydration 完整性传播（B2）：pending owner 保全、有界/校验卡片读取、删除许可=根完整∧元数据完整。[B 线回执](reviews/2026-09-14-backend-review-6eda2474/B_LINE_RECEIPT.md)
- A 线 session 终态事实化/批次硬界/每会话锁、grace 退出时重置、MCP 分页发现。[A 线回执](reviews/2026-09-14-backend-review-6eda2474/A_LINE_A1_A2_A3_IMPLEMENTATION.md)
- C 线 KV 接线与真实链路 wire 验收（本地 HTTP 捕获，非供应商校验）。[C 线回执](reviews/2026-09-14-backend-review-6eda2474/C_LINE_C1_C2_IMPLEMENTATION.md)
- 阶段收尾旅程：各环映射到已执行的全绿回归。[旅程回执](reviews/2026-09-14-backend-review-6eda2474/STAGE_CLOSING_JOURNEY_RECEIPT.md)

## 当前限制（如实）

- 跨进程连续任务轨迹已由 `host_process_variant` 证明（真实两 OS 进程、同 TaskId/lineage 恢复、指令传递证据）；仍未覆盖 watchdog/监督重初始化的全部路径。
- 本地 HTTP 捕获只证明客户端发出了字段；端点 schema 接受、实际命中、任务净成本下降均未验证（T6/T8；V6 工具结果块类型是 W3 待修项）。
- 冷目录分页与旧路径的跨层缺口未收口：scope 退休可漏未加载冷页引用（V1）、必需正文可被误报 Missing（V2）、service 边界丢 coverage/续查（V3）、续查状态可膨胀与 ABA（V4/V5）、取消丢已知用量（V7）——W1–W4 队列见 [NEXT_TASKS.md](NEXT_TASKS.md)。
- **TUI 作为操作入口的完整性（`d92564bc` 审查）**：U1–U7 均已实现并提交；`3bdb269c` 续审确认 U3 引入的 R1 回归已随 R1–R8 关闭（见上「续审基线」），后端侧 B1（批量 required 冷解析互相驱逐）与 B2（existing card 仅凭 `exists` 认领）亦已关闭（`de6bf061`／`2482f3ef`，回执见 [B1/B2](reviews/2026-09-16-review-3bdb269c/B1_B2_THIRD_BATCH_RECEIPT.md)）。
- **A 线残余（`d92564bc` 审查，已记录不回退）**：`/done` 的身份校验是前置快照比对而非原子保证（需给共享 `RuntimeCommand::CompleteTask` 加 expecting 变体）；`display_width` 为内联宽字符表；结果卡归档上限 8 张、不能按 TaskId 查任意历史；`view_partial` 未覆盖 live `Lagged` 之外的缺口；真实 PTY 端到端未执行。
- **已知抖动（不新增门禁）**：`host_t7_journey::named_pipe_t7_same_task_full_backend_journey` 的 `wait_file_content` 用 30s 墙钟截止，满载 Windows runner 上曾超时（run `35103897272` attempt 1；attempt 2 绿，本机 9.54s）。若再次出现，先看该截止而非假定功能回归。- 正式 `agent-host` 未指定策略时仍默认 Rolling；Dynamic 是可选实现。配置依据 [CONFIGURATION.md](CONFIGURATION.md)。
- 尚不能宣称：无限历史热内存有界、全部源码逐行审查完成、供应商 KV 已实测降低任务费用。真实模型实验按预算和凭据条件执行，不阻塞无须模型的生产接线。

## 按需阅读

架构边界：[ARCHITECTURE.md](ARCHITECTURE.md)；上下文规则：[CONTEXT_LIFECYCLE.md](CONTEXT_LIFECYCLE.md)；恢复操作：[RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md)。只读当前任务相关部分。

历史报告及冻结证据保留原位置。旧 `state.json`（v2）只作导航/来源元数据，不参与当前派工。
