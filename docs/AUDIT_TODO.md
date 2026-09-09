# 缺陷分流

当前执行顺序由 [NEXT_TASKS.md](NEXT_TASKS.md) 前列决定：**2026-09-10 Agent 任务流程审查（基线 `fb1ec9c`，W01–W08）为当前队列**。此前队列全部收口：M17 N 系列（N0–N8）、并行三线 A/B/C、2026-09-09 核心续审 R01–R14 均已代码落地（关闭与 CI 确认记录见 [CURRENT.md](CURRENT.md)），不重开已关闭项；2026-09-10 W 系列表见下方，按路线文档建议顺序切片执行，不另建阶段。

2026-09-09 核心续审原文：[reviews/2026-09-09-core-audit-93c300d/REPORT.md](reviews/2026-09-09-core-audit-93c300d/REPORT.md)。

2026-09-09 审查原文：[reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)。

2026-09-08 审查原文：[reviews/2026-09-08-closure-audit-11afdd7/REPORT.md](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)（FINDINGS.json/LOCAL_PROBES.json 同目录）。
原报告与探针：[reviews/2026-09-06-deep-audit/REVIEW.md](reviews/2026-09-06-deep-audit/REVIEW.md)。
2026-09-07 续审原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)。
建议回归按 [TEST_MATRIX.md](reviews/2026-09-06-deep-audit/TEST_MATRIX.md) 补进现有 crate，不新建总门禁。
旧审计正文：`docs/archive/route-reset-12c8628/docs/AUDIT_TODO.md`。

## 2026-09-08 闭环审查（基线 `11afdd7` → N 系列）

部分源码静态审查（55 路径；新 GUI/客户端/宿主三子树全文；无本地工具链）＋远端 CI 观察（run `34148921895` 在 fmt 失败，构建/测试被跳过）＋隔离 OS 探针（UDS unlink/rebind、包内 symlink 读取、半帧字节解码）。标注「已复核」的行已于 2026-09-08 在本工作树 HEAD `11afdd7` 静态确认；其余为审查报告结论，实施前按工单要求现场复核。

| 发现 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **F01（→N0，已复核）** | `agent-host/src/lib.rs:234` 无条件引用 `winpipe::serve`，`:503` `mod winpipe` 仅 `#[cfg(windows)]`；host_e2e 的 Unix 辅助无真实 UDS | 两平台明确 cfg 分支/受支持声明；agent-host 入 Linux CI，.NET build/test 入 CI | 不把 CI fmt 失败伪称编译失败或通过 |
| **F02（→N1，已复核）** | `agent-host/src/winpipe.rs:186` 每个实例创建都带 `FILE_FLAG_FIRST_PIPE_INSTANCE` | 首实例独占仅用于名称占用检查，后续实例正常模式；RAII 句柄，拒绝分支不手动二次 CloseHandle | 不删除当前用户 DACL/客户端令牌检查/远程拒绝 |
| **F03（→N1，已复核）** | 连接接入 install session，退出只 drop router 不 revoke；64 上限 `.expect` | session grant 归属连接 guard，全部退出路径 revoke；install 错误受控拒绝不 panic | 不用并发连接上限证明会话表不会耗尽 |
| **F04（→N1）** | accept 循环无停止通道；Ctrl-C 后 join 服务线程可能永久阻塞 | 显式停止信号＋连接集合关闭＋服务失败回执，有界 join | 不建第二调度器；沿用 RuntimeInstance.shutdown |
| **F05（→N1，已复核）** | `lib.rs:187/251` bind 前无条件 `remove_file`；默认 `/tmp` 固定名 | 用户私有、按工作区区分的端点；检查类型/所有者；只清理可证明属于自己的 | 不删除 SO_PEERCRED/chmod 与 Workspace 独占日志等既有缓解 |
| **F06（→N3，已复核）** | `lib.rs:425` `work.subscribe` 握手后 `Ok((response, _receiver))` 丢弃事件 receiver；客户端 Dispatch 分型前要求 `request_id` | 宿主持有订阅到连接关闭，单一有界 writer；notification 按 kind 验证；GUI 消费类型化事件 | 不新建事件平台；DTO 已存在（WorkEventNotification/work/event） |
| **F07（→N2，已复核）** | `ResumableSession.RunAsync` 连接异常后重连并重试 operation；submit/continue/cancel 全走它 | 查询/修改重试分离；未知修改返回 Unknown＋查询重同步；修改绑定 task/turn/generation＋宿主 incarnation | 审批答复不自动重试的既有行为保留；不建通用幂等数据库 |
| **F08（→N2）** | `Fault()` 只失败 pending 不关流不标终态；半帧写入失败不毒化 | 单一终态故障路径：标 Faulted、拒新请求、结清 waiter、关闭传输；半帧写入失败毒化连接 | 完整写完后的本地等待取消不冒充服务端取消 |
| **F09（→N2/N3）** | `LiveAsync` 连接/握手在锁外；快照完成前发布 fresh；Snapshot 后 Subscribe(null) 忽略回执；Dispose 无代际约束 | single-flight connect；安装前校验 generation/disposed；快照与订阅同一 run/host 身份衔接 | router 已有 barrier/resync-only，不承诺无限历史重放 |
| **F10（→N5，已复核）** | `agent-host/src/main.rs:207-213` `--restore-latest` 按文件名枚举后直接 `serde_json::from_str::<RuntimeCheckpoint>` | 统一用 CheckpointStore `decode_checkpoint_file/bytes`＋完整 `RuntimeInstance.restore` | 不改检查点格式；raw JSON 兼容入口不替代正式产物路径 |
| **F11（→N3，已复核）** | `work.rs:465` `validate_text` 拒绝一切控制字符（含 LF/TAB）；GUI AcceptsReturn=true | 短标题与有界完整正文分离；正文允许合法换行/制表；身份/路径仍严格 | 不删除全部输入验证；跨语言按契约统一标量/字节口径 |
| **F12（→N4/N5）** | 待审批快照只有 request_id＋call_name；快照缺计划/结果/工件引用 | 审批经既有 gate 提供受限详情＋绑定有效性；补 GUI 实际使用的计划/执行状态/结果投影 | 权限决定仍归 Core/gate；不从通用文字反推 |
| **F13（→N4/N7，已复核）** | 每 3s 刷新重建审批行，命令加入长寿命 `AsyncCommandGroup` 不移除（推导 1h≈2400 引用） | 按 request_id 复用稳定行/命令，移除时撤销注册；刷新 single-flight＋代际；关闭释放 | 不换 GUI 框架；对象生命周期先于框架更换 |
| **F14（→N7，已复核＋已落地 2026-09-09）** | MetricsSession Windows 无 parent 仍报 whole-tree；Linux 提前标 seen 可能漏孙进程；末样本标 idle；`_samples` 无限追加。**已落地（`87850ae`）：单次快照建父子关系＋一次全局去重（共享后代只计一次）；覆盖标注 root_only/full_tree/unknown（Windows 无 parent 枚举即 root_only，绝不伪报全树）；采样走有界环、count/max/last 为运行聚合；idle 仅由显式 `MarkIdle` 写入（`MetricsReport.Final` 与 `Idle` 分离）** | 覆盖范围 root_only/full_tree/unknown；一次快照建父子关系＋去重；有界采样环 | 不否定独立人工测量；不填假全树值 |
| **F15（→N6，已复核）** | 决策 supersession 按实体/子串重合排队 `Superseded`；无同任务/决策键/显式替代约束 | 实体匹配降为相关性；仅明确替代目标＋正确任务范围进终态；否则保留两条按 attention 冷却 | 不重开已修好的同任务同 probe 验证关联；Runtime 用户约束权威未被删除 |
| **F16（→N6，已复核）** | `residency.rs:315-316` Warm 路径查 keep_alive/lease；Resident TTL 路径（`gc/minor.rs`）无此检查 | 跨层共用到期保护；lease/keep_alive 范围明确；终态不可复活 | 不重调 GC 参数；不引入新淘汰算法 |
| **F17（→N6，已复核）** | `plugin.rs` `skill_read` 词法相对检查后普通 `File::open`；symlink/junction 可指包外（探针证机制）；FIFO 可在 take 前阻塞 | 复用既有 ConfinedDir/受限普通文件句柄；拒绝链接/非普通文件 | 前提是操作者安装启用的包树存在此类文件；保留双激活门/64KiB/来源版本 |
| **F18（→N6，已复核）** | `engine.rs:1705/1710` `to_summaries` 先全量投影再 `bounded_catalog`（limit=0 也投影） | limit0 早退；惰性投影或选中 ID 后复制；保持既有稳定顺序 | 惰性投影不自动把扫描 CPU 降为 O(limit)；不接数据库 |
| **F19（→N2/N3）** | `SendAsync` 只验 envelope，不调用具体 payload.Validate；反序列化缺字段用默认值 | 每个类型化 API 发送前/接受后运行 validator；两语言口径一致 | 不建反射式通用验证框架 |
| **F20（→N0/N8）** | 半帧样本 `00000009`（LE=150994944，先触发超长帧拒绝）；交错测试是两条连接；interop retry 用新 key；host 线程 join 错误被忽略 | 修准现有用例：合法长度半帧、单连接乱序、原 key 重试、执行后丢 ACK、服务线程结果必须检查 | 不建第三套 harness；测试全绿≠路径已验证 |

审查同时确认**不再原样重报**的已修复项：监督身份化台账/类型化对账/确认式清理、proof 监督接线、metadata 发布围栏、原子 StartWork、同任务同 VerificationProbe 验证关联、skill_read＋MCP/Plugin 配置缝（见 CURRENT「已修好」段与 2026-09-07 表的关闭记录）。

## 2026-09-09 三线审查（基线 `bbf7f5d` → 并行三线 A/B/C）

静态审查＋远端 CI 观察＋隔离 Python 探针（无完整 checkout——克隆仍败于 DNS、无 Rust/.NET 工具链、未运行仓库构建/测试；20 个 crate 目录树核对、28 路径重点阅读其中 14 全文；探针不是回归测试）。标注「已复核」的定位已于 2026-09-09 在本工作树 HEAD `7e026ee` 静态确认；其余为报告结论，实施前按工单现场复核。基线后 `9fb2030`/`433d21e`/`843803f`/`7e026ee` 四提交已处理 C 线三项与 admit 测试断言（随 N4 验收，不重开）。切片归属见 [NEXT_TASKS.md](NEXT_TASKS.md)「并行三线」节。

| 发现 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **GC-DEL（→A1，已落地 `6f47a90`，测试 `df5b972`）** | 基线：`commit_full_gc` 返回 `blobs_to_delete`、engine 随后删 blob，删除先于 Runtime 回合的 durable barrier。**已落地：`commit_full_gc` 不再返回删除清单——Context GC 只外置（Warm→Cold→External 引用），物理删除唯一经 Storage GC（`plan_storage_gc`/`commit_storage_gc`，按语义死亡＋保留与引用规则）；recall 后 blob 留盘，由 startup reconcile 的陈旧副本规则对着实际恢复的 state 收回（`gc_bounds::recalled_blob_survives_until_the_reconcile_reclaims_it`）** | 驻留迁移与物理删除分离：正文可恢复保留，证明不再被当前/仍受支持检查点引用后才交 Storage GC；复用现有 checkpoint 保留窗口与 store | 不新建数据库；不把「内存里有一份」当旧恢复根可失效；崩溃恢复路径未随本切片重跑，不写成已实现保证 |
| **GC-CANCEL（→A1，已落地 `6f47a90`，测试 `df5b972`）** | 基线：GC 外置计划取走 `pending_externalize_retry` 与 Warm 溢出条目后等待异步写入，future 被丢弃时条目不归还运行状态。**已落地：条目从不离开 state——溢出与 retry 全部留在 `pending_externalize_retry`，plan 只携带 id＋锁内预序列化字节；IO 阶段被丢弃（取消/panic）至多丢已落盘写入，条目由下次 pass 同列表重试（`gc_bounds::dropped_externalize_plan_keeps_items_owned_by_the_state`）；写为 temp+rename 原子，半写 blob 不存在）** | 提交前源条目保持可追踪 owner，或窄范围 pending 事务/守卫 | 不把「没持锁跨 I/O」当「取消安全」；这是构造性归还，非已证明 `/cancel` 必达此调用 |
| **QUARANTINE（→A1，已落地 `6f47a90`，测试 `df5b972`）** | 基线：checksum 不符→quarantine rename 失败→相关 ID 仍进 owner 移除集合，下次扫描把原路径坏文件当无主可重建、建立新 checksum 基线。**已落地：`quarantine` 返回明确结果（是否真正移动）；rename 失败保持正式路径并保留 owner——完整性拒绝不被隔离失败消解（`store.rs` 测试 `quarantine_failure_keeps_the_owner_and_the_rejection`），成功迁移/缺失/损坏/IO 失败在 reconcile 报告分开计数** | 隔离返回明确结果；失败保留原 owner 或显式拒绝/隔离态；成功迁移/缺失/损坏/IO 失败分开处理 | 一次完整性拒绝不能因隔离失败变成下次更容易接受 |
| **ADMIT-TERMINAL（→A1，已落地 `6f47a90`，测试 `df5b972`）** | 基线：`directive.rs` 准入终态 Warm 条目先从 buffer `remove` 再 semantic 拒绝。**已落地：liveness 判定前移——`plan_admit` 对 Warm/外部条目先判 `is_live`/可检索性，拒绝发生在任何迁移之前（拒绝不是迁移）；`admit_refusal_for_terminal_warm_item_keeps_it_in_the_buffer` 锁定 buffer 原状** | 验证在迁移前；拒绝路径不改变记录、索引和归属 | — |
| **ADMIT-LEASE（→A3，已落地 2026-09-09）** | 基线：`residency.rs` 准入较老、仍 Live 的 Stored Ephemeral 条目保留旧创建时间且无新有限使用期，下次维护可能立即 TTL 终结（旧实现靠刷新生命周期时间戳让条目显新）。**已落地：`reenter_working_set` 不再刷新时间戳——创建时钟保持原值，准入授予有界使用期租约（`state.turn + max_lease_turns`，与模型 `context.lease` 同一上限；更长未到期租约不缩短），residency 两条终结路径共用 `protected_from_expiry`；回归：租约期内完整 residency 机不 TTL 终结、到期后正常老化终结、created_tick/created_turn 不变** | 保留创建时钟＋明确准入租约/有限使用期；不支持时一开始就拒绝 | 不篡改原始创建时间让条目显新；创建身份/最近访问/当前准入/到期保护是不同时钟 |
| **RANGE-PARTIAL（→A2，已落地 2026-09-09）** | 基线：正文裁剪到 `max_item_chars` 后原始行范围保留；`gc/reachability.rs` 覆盖判断先行比较行范围包含；`materializer.rs` 的 `partial_body` 只反映本轮装配裁剪，required 路径存在直接标非 partial。**已落地：同 revision 覆盖证明要求新正文未被引擎裁剪（声明区间是工具报告区间，裁剪正文只保留前缀，落入字面包含守卫即拒绝，保守共存）；selected/foreground 的 `partial_body` 如实反映摄取期裁剪（descriptor 是身份卡永不 partial）；required 可见副本仅当非 partial 或恰好暴露 overlay 会嵌入的内容时满足 claim。回归：entity.rs（裁剪正文不作覆盖证明）、consumption_truth.rs（摄取裁剪入选即 partial）、required.rs（partial/required 传播）** | 区分资源 revision／工具返回区间／当前保留正文与区间／表示类型（原文/摘要/描述符/partial）；先修覆盖证明与 partial/required 传播 | 不新建 Frame 系统；未证实为权限绕过，完成门禁影响需端到端回归（未做） |
| **ROLLING-PRIOR（→A3，已复核＋已落地 2026-09-09）** | 基线：`take_fold_job` 的 `prior` 从空串起只收集本次移出 records；旧摘要只参与身份（`summary_id`），不进 `compact_fold` 的 `CompactionRequest.source`，压缩后覆盖 `state.summary`。**已落地：折叠输入 = 旧摘要正文＋本次移出记录（有界），来源覆盖记录合并；`FoldRestore` 守卫在压缩失败/丢弃时把记录还回工作集；`compact_fold` 失败不覆盖旧摘要；EchoCompactor 确定性验证第二三次折叠仍可达第一轮独有约束（`later_folds_merge_the_prior_summary_into_the_next_input` 等）** | 旧摘要＋本次折叠内容组成下次输入并记录来源覆盖；折叠失败/取消保留原状态；先用确定性 compactor 验证第二三次折叠仍可达第一轮独有约束 | 正式宿主默认 Rolling，不能当离线基线问题后置；摘要质量不在本切片范围 |
| **ROLLING-FOCUS（→A3，已落地 2026-09-09）** | 基线：Rolling 不跟踪 focus，活动任务检查点恢复 fail-closed。**已落地：`RollingState.focus` 跟踪 FocusChanged（generation 递增）/FocusCleared/TaskCompleted；diagnostics 暴露 `focus_task_id`/`focus_generation`；checkpoint/restore 携带 focus（`rolling_tracks_focus_for_the_restore_authority_check`）；`host_restore` 集成改用默认 Rolling profile 验证活动任务检查点恢复（A 提交树 3/3），不再 fail-closed 拒绝** | 与默认产品配置一起解决（即 N5 backlog 行） | 信封解码已修 ≠ 默认 profile 冷恢复闭环（正式冷恢复声明仍等 N5 验收） |
| **SNAP-GAP（→B1，已复核＋已落地 2026-09-09）** | 客户端先快照后订阅：不带快照游标、不用订阅回执 watermark/resync；服务端订阅自建较晚切点并过滤不高于该切点的事件，中间段变化两头都不可见。**已落地：客户端握手改 subscribe→snapshot 次序（宿主 receiver 注册先于其快照屏障），泵按快照 watermark 对 durable 事件去重，重连安装即清空会话级队列（有序复位边界）；无 wire 变更** | 同一切点的快照＋订阅，或先注册缓冲事件再取快照、按该切点去重 | 不承诺无限重放；不建 Chronicle；沿用 resync-only 边界 |
| **LIVE-DELTA（→B1，已复核＋已落地 2026-09-09）** | live-only delta 重复前一 durable seq，不能以 durable watermark 一刀切过滤；跨重连停止旧 producer 不清队列中已存在的旧代事件。**已落地：契约 `RuntimeEvent::is_live_only`（ModelDelta/ModelRetrying）＋宿主转发器 durable/live-only 分流（live-only 恒转发，durable 仍按切点去重；`assert_notification` 同步放行游标重复）＋客户端重连复位** | live 与 durable 分流；有序 epoch/reset/snapshot 边界 | — |
| **QUEUE-COMPLETION（→B2，已复核＋已落地 2026-09-09）** | `clients/dotnet/Agent.Client/BoundedEventQueue.cs:54` 字段初始化 `_completion = NewCompletion(null)` 立即 `SetResult()`——队列可继续写入时 `Reader.Completion` 已完成，关闭又换新任务。**已落地：构造即未完成的同一 TCS，`TryComplete` 后仅当 backlog 排空才结算（最后一次 `TryRead`/`Clear` 触发；空队列关闭立即结算；错误延迟到排空后 fault），对齐 `ChannelReader<T>.Completion` 语义；dotnet 客户端测试 72/72（提交前重跑，计数含并行线当日新增），含 5 项 Completion 回归** | 保留同一未完成 TCS；写端关闭且 backlog 排空后才完成/失败（对齐 `ChannelReader<T>.Completion` 语义） | 不为此重建客户端队列框架 |
| **CANCEL-ALL（→B2，已复核＋已落地 2026-09-09）** | `agent-host/src/lib.rs:332` `cancel_all` 先清登记表再调取消 hook，`:339` `wait_empty` 用已清表判断结束；主程序主要等 Ctrl-C，服务线程提前失败不收敛；`composed.shutdown().await?` 失败可能跳过传输停止。**已落地：`cancel_all` 只发中断不再清表（worker 自删表项，表锁外调用 hook），`drain` 的第二段 `wait_empty` 观察真实退栈、超时留可见残余；main.rs 以 select 等待「Ctrl-C 或 serve 线程先退出」，`composed.shutdown()` 结果捕获、传输停止不再被失败跳过；宿主单测 2 项（中断不清表、wedged 残余可见）＋hostile-clients e2e 现在真实等待 worker 退出与 grant 释放** | 停止请求 ≠ worker 退出 ≠ grant 释放 ≠ 宿主关闭完成；确认完成再删 owner；超时留明确未确认结果 | 不用清空登记表冒充完成；不提前删 owner |
| **RETYPED（→B2，已复核＋已落地 2026-09-09）** | `retyped` 验证前把请求 `work` 字段清成 `None`，可能把应拒绝的 run-scoped 信封清洗成合法请求。**已落地：`retyped` 原样携带 `request.work` 交 router 既有验证器；单测验证保留字段＋validator 拒绝，e2e：携带 work 的 run-scoped 请求收到 `protocol.request_invalid` 结构化拒绝、连接保持可用、goal 未被受理（named-pipe 本地绿；unix 变体随 CI Linux job）** | 验证原始信封，或保留字段交既有 validator | 非权限提升结论；会话授权仍由宿主安装 |
| **GUI-EVENTS（→C1/C2，基线后已落地）** | 基线时点：主 ViewModel 未消费事件流、审批只有 request_id＋call_name、每 3s 重建审批行且命令积累、默认 FixtureLayout | `9fb2030`（F12 快照风险＋目标摘要）/`433d21e`（客户端镜像）/`843803f`（桌面真实事件＋稳定行＋诚实状态）已落地；随 N4 工单验收关闭 | 不重复立项；N3「事件到达客户端」≠N4「真实输出进入工作台」，验收按 N4 检查项 |
| **ADMIT-TEST（→A1 附带，已落地 `6f47a90`）** | 基线：`tests::admit::admit_store_read_does_not_block_unrelated_context_work`：32 MiB 输入经默认正文裁剪不保证真有慢 I/O，再以「admit 耗时＞diagnostics 三倍」相对耗时推断无持锁；run `34271105841` Windows 全测试红（diagnostics 1.9ms vs admit 1.5ms）。**已落地：确定性屏障 `IoBoundaryPause`——admit 停泊在外部读边界（state 锁已释放），断言 diagnostics 在停泊期间完成、放行后准入结果正确；超时仅死锁兜底，不再做相对耗时推断** | 确定性屏障：确认 store read 进入→暂停→验证 diagnostics 可完成→放行→验证准入结果；超时仅死锁兜底 | 不放宽阈值；这次红不能证明生产持锁跨 I/O，偶然绿也不能证明没有 |

## 2026-09-09 核心续审（基线 `93c300d` → R 系列）

全仓范围续审（20 crate 目录清点；核心状态、上下文与工程边界按风险深读；14 项：7 P1、7 P2，其中 13 项动态反例、1 项静态调用链确认；审查环境无本地工具链，反例在隔离复制源码上运行）。原文与证据：[reviews/2026-09-09-core-audit-93c300d/REPORT.md](reviews/2026-09-09-core-audit-93c300d/REPORT.md)。执行顺序按报告建议：R01 先行，随后 R02/R03（A 线 owner 与恢复根）、平台项 R06/R07/R13/R14 按既有 B/C 所有权；R04/R05/R11/R12（tool-runtime/workspace）随核心线排片。每个切片独立交付，不把「审查清零」设为新阶段。

| 发现 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **R01（P1，已落地 2026-09-09）** | 基线：`actor/safepoint.rs` `accrue_checkpoint_debt` 仅按枚举去重、两条 ACK 路径按原因集合做差，冻结后的同类修改被第一个 ACK 误清；`actor/turn.rs` 终结屏障只等待已有写入、无补捕获。**已落地：冻结批次与可积累欠账分离——safe point 冻结时 `mem::take` 把欠账移入 in-flight artifact 的 `captured_debt`，成功 ACK 只退休它冻结的集合，失败路径（settled/await/schedule 各错误分支）把冻结集并回活欠账；被冻结的 reason 可再积累，留给下一个快照。屏障（`finalize_turn`、`record_completion_commit_failure`）退出证明无未捕获欠账：await 成功且仍有欠账时最后一次捕获；失败写不内联重试，欠账恢复并 fence 给下一个 settled batch。回归 `same_reason_debt_accrued_during_background_save_survives_the_ack` 固定交错（两快照各自冻结、第二工件带新 anchor 内容、恢复后 revision=2、continuation gate 全程诚实）；既有 `failed_checkpoint_write_fences_continuation_until_a_retry_lands` 保持绿。报告未覆盖范围的两个静态候选已核：单飞行不变量＋单调 watermark 使旧 ACK 覆盖新欠账不可达；checkpoint 聚合字节上限按既有 oldest-first 裁剪（`aggregate_byte_budget_prunes_oldest_first` 在测）。实际检查：turn 115＋lib 368＋actor 72＋instance 31＋host_restore 3、`cargo fmt`、clippy（agent-runtime 无警告）全绿** | 持久化债务绑定产生它的快照身份（冻结集分离即实现）；两条 ACK 路径共用退休规则；barrier 退出证明无未捕获欠账而非仅 JoinHandle 结束 | 不建第二套代次数据库；沿用 RuntimeActor 编排与 Core 提交/恢复权威；屏障失败不自判可恢复 |
| **R02（P1，已落地 2026-09-09）** | 基线：`store.rs` 强引用根收集只链 resident+warm、`gc/full/mod.rs` pass 条件不含 pending——pending 外置记录漏入强引用根，其仍引用的证据被 Storage GC 删除；仅剩 pending 时还停止重试。**已落地：Storage GC 引用根 `.chain(pending_externalize_retry)` 补齐 pending 所有者（`Delete ∩ Reach_strong(all_retained_roots) = ∅` 保持，沿用既有强边思路、无全表扫描）；GC pass 条件加入 `pending_externalize_retry.is_empty()`——仅剩 pending 时 pass 仍运行并驱动重试；pending 计为第四个逻辑目录位置：catalog 新增 `Pending`（search/fetch 投影为 warm 语义，仅 Storage GC 与维护 pass 消费差异），`has_exactly_one_owner`/checkpoint restore 验证/diagnostics `total_items`/`inspect` 摘要/access 打戳/store 检索投影全部覆盖。回归三项：safety `storage_gc_keeps_evidence_referenced_by_a_pending_retry_owner`（Resident 对照组与 pending 实验组同样保留证据，走真实 blob 删除候选路径）；liveness `a_pending_only_state_still_runs_the_pass_and_lands_the_retry`（仅 pending 时 plan 必须生成且 IO 恢复后重试落地）、`pending_owners_retry_after_the_store_recovers`（故障 spill→checkpoint/restore 保持 pending owner→目录可见可 fetch→IO 恢复后 drain 进 external map）。实际检查：`cargo test -p context-simple` 314/314（含新增 3 项）、`cargo fmt -p context-simple -- --check`、`cargo clippy -p context-simple --all-targets` 无警告** | 根集合补齐 pending 所有者；IO 恢复后 pending 必须能进维护并取得进展 | 不反复全表扫描；已沿用的工作强边思路可继续 |
| **R03（P1，已落地 2026-09-09）** | 基线：`context-simple/store.rs:1439–1450`：reconcile 因新快照已有 Resident 副本删除 blob，破坏仍受支持的旧快照恢复（restore B → 清理 → restore A → fetch None）。**已落地：删除判定纳入保留 checkpoint 的恢复根——`ContextEngine::checkpoint_recovery_item_ids`（引擎从 checkpoint 数据提取 external 正文 id）＋`reconcile_store_protecting`（契约默认转发，context-simple 实现；受保护 id 的 blob 即使当前 resident 也不删除、不重建，原因行「retained checkpoint recovery root」）；runtime 恢复后 `collect_checkpoint_recovery_roots` 扫描 CheckpointStore 全部保留 artifact（load_verified＋decode_checkpoint_bytes），并集去重后传给 post-restore reconcile。回归三项：store 级反向（受保护不删＋reasons 可解释）与对照（无保护照旧回收）；引擎级完整探针序列（外置保存 A→Admit 回 resident 保存 B→restore B＋受保护 reconcile→restore A→fetch_external 恢复正文，正是审计「restore B→清理→restore A→fetch None」的反例）。实际检查：context-simple 317/317（含新增 3 项）、agent-runtime lib 371、workspace 编译全绿、fmt/clippy 干净** | 物理删除检查全部保留者（含仍支持恢复的 checkpoint）的强引用闭包；磁盘保留多份可恢复副本不是缺陷 | 不把「新快照有副本」当旧恢复根可失效 |
| **R04（P1，已落地 2026-09-09 `d610d20`）** | 基线：`tool-runtime/supervision.rs:79–91`：锁把单次退避量当总等待量，超时条件永远不可达（两平台持锁 4 秒仍等待）。**已落地：`retry_flock` 区分单次退避与总预算——`LOCK_WAIT_TIMEOUT=2s` 独立 deadline，退避 25ms→×2→250ms 封顶，锁持续被持有时等待者在预算时刻真实超时返回。回归两项（预算内持锁超时、锁提前释放成功）** | 区分单次退避与总等待预算 | 不新建监督框架 |
| **R05（P1，已落地 2026-09-09 `d610d20`）** | 基线：`tool-runtime/tools/session.rs:81–88,555–561`：session 输出 EOF 被当成退出、poll 无期限等待活进程且不响应取消、持有全表锁。**已落地：EOF 不再判定退出（仅 `child.try_wait()` 判定）；poll 有界（`EXIT_DRAIN_TIMEOUT`）且响应取消；poll 短锁快照后释放全表锁再结算/持久化。回归三项（活进程输出 EOF 不报退出、poll 响应取消且有界、失败 start 不消耗容量）** | EOF≠进程退出；poll 有界且响应取消；锁范围收窄 | 不重建 session 工具 |
| **R06（P1，已落地 2026-09-09 `b2badef`）** | 基线：`Agent.Client/WorkDto.cs:306,587`：合法 2,001 字符 goal 不能被 .NET snapshot/task detail 接受（scripted wire submit 接受、两次 snapshot fault），后续握手持续失败。**已落地：`WorkSnapshotResponse.MaxGoalChars` 从 2,000 对齐宿主 `MAX_SNAPSHOT_GOAL_CHARS` 的 200,000（与 `WorkSubmitRequest` 共享常量）；新增 conformance 测试证明 2,001 字符在 snapshot＋task detail 通过、200,001 仍拒绝。实际检查：dotnet client tests 88/88（含 R06 conformance）** | 按共享 C0 契约统一长度口径；两语言验证同界 | 不在 GUI 侧二次截断 |
| **R07（P1，已落地 2026-09-09 `df03c7d`）** | 基线：`MainWindowViewModel.cs:736–749,790–795`：并发刷新或已发请求超时清掉未知提交的幂等键；同目标重试变新受理身份。**已落地：`Pending(k)→Unknown(k)` 保留 k——决定性协议拒绝才清键，超时/运输故障转 Unknown 且保留 `client_request_id` 供重试复用。回归：并发刷快照＋超时后仍保留 outstanding submit key** | `Pending(k)→Unknown(k)` 仍保留 k；`Unknown(k)→已受理/已拒绝` 必须有对应 k 的证据 | 不建通用幂等数据库 |
| **R08（P2，已落地 2026-09-09）** | 基线：`context-baselines/rolling.rs:156–180`：Rolling 仅给压缩器 2,000 字符却移走整批旧记录并宣称覆盖，未读尾部也退出工作集。**已落地：按输入容量诚实消费——`take_fold_job` 以 `SUMMARIZER_PRIOR_CAP` 为硬界：能**完整**进入压缩输入（旧摘要前缀＋最旧记录）的记录整条折叠（`F ⊆ I`）；装不下的记录按容量**切分**——前缀进本轮输入（partial coverage），残余尾部同 id 写回工作集队首，下轮继续消费（`F ⊆ I ∪ U`）；全部放不下（无剩余容量）则不折。切分仅在压缩成功后提交（失败守卫保持整条原样）；覆盖声明（`summary_source`）区分完整/部分消费、注明残余仍在工作集，`PartialFold` 携带 `content`/`tail`/`index`。回归三项：全部超限时两条记录各产生 partial 过渡且前缀可达；部分超限时小记录折叠进摘要、大记录部分消费＋摘要含小记录；agent-replay 滚动预算场景保持绿（切分使 Rolling 恢复压预算）。实际检查：context-baselines 13/13（含新增）、agent-replay scenarios 10/10、fmt、clippy 干净** | 记录压缩器真实消费输入与可恢复残余（移出的正文 ⊆ 已消费输入 ∪ 可恢复残余）；诚实区分覆盖保证与摘要语义 | 摘要质量不在范围；不重开算法研究 |
| **R09（P2，已落地 2026-09-09）** | 基线：`prompt.rs:556–580` 生成 hint、`materializer.rs:836–850` 的 descriptor 判断只看 `path@revision`——`fs.read` 保留的 start_line/end_line/covers_file 同时被忽略，同版本不交叠区间被误当作相同正文（100 行历史 body 从可见变成只有 descriptor）。**已落地：共享契约新增 `FileBodyWindow`（path＋revision＋start/end＋covers_file）与 `visible_body_windows_cover`，统一规则 `same(path,revision) ∧ historical_interval ⊆ union(visible_intervals)`；未知范围不构成覆盖证明，仅 `covers_file` 窗口可覆盖无区间记录；`ContextHints.visible_body_windows` 携带请求已有窗口，`MaterializedItem` 透传 item 区间。prompt 组装（`visible_body_windows_for_request`/`visible_body_windows_from_parts`）与去重（`omit_selected_file_body`）同 materializer descriptor（`price_as_file_body_descriptor`）消费同一函数。回归：契约层区间测试（同版本包含/不交叠/异版本/未知范围/合并相邻窗口），prompt 层反向（同版本 L101–200 窗口不隐藏 L1–100 历史正文）与对照（包含窗口仍照常去重），既有 dedup 测试以 `covers_file` 整文件读取补 metadata 后保持绿。实际检查：agent-contracts 164、context-simple 317、agent-runtime lib 373（含新增 2 项）、workspace 编译、fmt、clippy 干净** | 历史区间 ⊆ 同版本当前可见区间并集的集合包含证明 | 不引入向量库或新检索栈 |
| **R10（P2，已落地 2026-09-09 `fe901b7`）** | 基线：`actor/model.rs:1248–1255,1377`：Schema 过滤仅影响执行快照，实际请求仍向模型展示被移除工具（静态调用链确认，未动态探针）。**已落地：schema profile 编译前移到输入装配/预算计算/Ready 报告之前——plan specs、report 选择与 request tools 命名同一最终集合；MustSurface 被拒为显式不可满足拒绝（命名的 Error 事件＋turn 无 fencing 收尾）而非静默丢弃工具；可选拒绝成为 Unavailable 省略行；provider 预算裁剪后丢弃过期 profile，使 snapshot profiles 与其 specs 精确一致。回归：3 单测＋2 actor e2e** | Ready 声明与执行集合一致 | 不把 ToolSpec 字段当权限 |
| **R11（P2，已落地 2026-09-09 `d610d20`）** | 基线：`tool-runtime/tools/session.rs:323,336–341`：session start 早退泄漏 Pending 槽，16 次失败后无进程也不能再启动。**已落地：槽位预留移到同步审批之后、spawn 之前；spawn/无 pid 失败路径显式移除槽位。回归：`failed_starts_do_not_consume_session_capacity`** | 早退路径归还/释放 Pending 槽 | — |
| **R12（P2，已落地 2026-09-09 `b2badef`）** | 基线：`agent-workspace/lib.rs:1437–1443`：after_tx 在同事务 Prepared 处越过游标又返回同 ID 的 Committed，增量读取不前进。**已落地：read_changes 的 after_tx 游标按整事务命名——共享 tx_id 的每一阶段（Prepared/Committed）都排除，仅返回严格更新的记录；新增回归覆盖 cursor-by-newest-commit（返回空）与 cursor-by-Prepared（同事务 Committed 不泄漏）。实际检查：agent-workspace lib 106/106（100 既有＋R12 回归）** | 游标推进以 Committed 可见为准 | — |
| **R13（P2，已落地 2026-09-09 `df03c7d`）** | 基线：`Agent.Client/ResumableSession.cs:267–281`：事件队列 overflow 后永久 completed；snapshot/底层重连成功也无法恢复新事件。**已落地：队列可重建——overflow 标记当前世代单调关闭，成功重连时重建全新活队列（丢失 backlog 只重新快照、不重放），重读 `Events` 可收到新事件；GUI `Resynced` 处理器在旧泵已结束时重启事件泵。回归：`Overflowed_session_rebuilds_its_event_stream_on_reconnect_and_delivers_new_events`** | overflow 后队列可重置或显式重建，重连后新事件可达 | 不承诺无限重放 |
| **R14（P2，已落地 2026-09-09 `df03c7d`）** | 基线：`MainWindowViewModel.cs:483–494`：每事件 UI.Post 把有界源转成无界 dispatcher backlog，输出上限过晚生效。**已落地：泵→UI 单飞有界 drain（最多一个排队回调）；源侧 delta 文本按 `MaxOutputBytes` 截旧；durable 事件源侧调度合并快照刷新维持可恢复语义。回归：`Paused_dispatcher_bounds_the_pending_ui_backlog_at_the_source`。dotnet test 91/91、build 0 警告** | 背压有界（丢弃/合并策略显式）；上限在源侧生效 | 不换 GUI 框架 |

审查同时确认的网络完整性证据留在 IO 附录，网络安全不作为本轮深入方向（用户 2026-09-09 指示）；未覆盖范围（各 crate 深读边界、未跑的恢复解析器与平台分支）见报告「覆盖范围与未覆盖部分」节，实施前现场补读。

## 2026-09-10 Agent 任务流程审查（基线 `fb1ec9c` → W 系列）

主审查者逐项复核调用链并运行隔离反例（8 项：4 P1、4 P2；未修改生产代码、未连真实 provider；证据层级与限制见 [EVIDENCE.md](reviews/2026-09-10-agent-workflow-fb1ec9c/EVIDENCE.md)）。原文：[REPORT.md](reviews/2026-09-10-agent-workflow-fb1ec9c/REPORT.md)；切片顺序与衡量方式：[WORKFLOW_AND_ROUTE.md](reviews/2026-09-10-agent-workflow-fb1ec9c/WORKFLOW_AND_ROUTE.md)。W 编号只是本轮定位，不另建阶段、不替代本表。执行顺序按路线文档：W01 → W04+W08（同一切片的两个半边）→ W02 → W03 → W05 → W06/W07（两个小切片）→ 三类真实任务衡量（条件性，具备真实 provider 条件才运行）。

| 发现 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **W01（P1，已落地 2026-09-10）** | 基线：`task.rs:1387–1399` `on_user_turn` 把 `turn_intent` 截为 2,000 字符；`actor/turn.rs:399–415` 用该字段构造 TaskContinuation，继续不重新 ingest 原正文，Rolling 又按 created_turn 排除当前用户输入——继续请求只见指令前缀（探针：尾部约束可见性 [true, false]）。**已落地：TaskRecord 新增 `current_directive`（`directive.rs` TaskDirective：RuntimeInputEnvelope 身份＋digest，正文为既有 sealed input artifact 引用，无 artifact workspace 的组成用字节有界 inline body；不复制第二份正文）——`apply_user_directive` 在上下文应用成功后同一 Actor 转换内安装预览＋完整身份；`continue_active_task` 先过既有 durability gate 再解析保留指令（task 归属/source/authority/kind/lifecycle/digest 结构校验，artifact 按 sealed locator 用原 run id 认证读取＋字节上限＋digest/preview 复验）；旧记录低于旧上限按 legacy inline 继续、恰在旧上限拒绝并要求重发（完整与截断不可分辨）；checkpoint 携带并验证 `current_directive`（旧检查点 serde default 兼容）；agent-replay 继续事件改按原 run 的 sealed input artifact 解析（owner+digest 校验），截断 legacy 记录不再用 preview 冒充正文。回归 4 项（`tests/turn/directive.rs`）：完整指令（2,100 字符＋中文尾部＋空白）在首次/继续/冷恢复后继续共三次模型请求逐字可达、checkpoint 预览≤上限且 body_ref 绑定原 input、继续不重复 ingest（Rolling 原文恰一份）、continuation envelope 的 causal_parent/body_ref/bytes 如实；inline 组成、artifact 缺失拒绝继续（无 TaskContinuationStarted、revision 不动）、legacy 恰在旧上限拒绝。实际检查：agent-runtime turn 119＋lib 373＋actor 74、agent-replay 59、host_restore 3、fmt、clippy 全绿** | 同一当前指令身份的继续保有同一完整正文或可验证、可读取的正文引用；`preview(directive)` 不充当 `directive`；2,000 字符只用于展示；回归检查整个最终模型请求与正文引用 | 保留指令不是重放副作用的授权（body reference 不恢复权限）；不为继续重新 ingest 新正文；不复制第二份正文进 TaskRecord |
| **W02（P1，已落地 2026-09-10）** | 基线：`actor/model.rs` `final_frame_body_key`/`record_final_pack_drop` 仅比较 `path@revision`；final packing 删正文后，只要剩同版本文件其他区间就认为原正文可见。反例（helper 级探针）：两份必需正文 L1–100 与 L101–200，删除 L1–100 后 `required_body_present=false`、`required_misses=0`。**已落地：`record_final_pack_drop` 的 still-visible 判定改用 R09 的区间包含规则（`visible_body_windows_cover`）——同 item_id、逐字节相同正文、或同 path+revision 且候选窗口（含非 partial 整文副本）覆盖被删记录区间的才算仍可见；partial 候选不构成覆盖证明，无 revision 不构成覆盖证明；删除字符串身份匹配。回归：互补区间删除即 required miss（报告反例）、整文副本保留时不误报、partial 副本不覆盖、相同正文不误报；既有 duplicate/prefers-optional 测试保持绿** | `required_body 被移除且没有同请求覆盖副本 ⇒ required_miss`——final packing 复用 R09 的范围包含判定（同 ID 部分副本、不同 ID 互补区间、未知范围），请求与 completion safety 一致 | 不到最后阶段退回较弱的字符串身份规则 |
| **W03（P1，已落地 2026-09-10）** | `actor/restore.rs` 恢复根只传给 `reconcile_store_protecting`；任务完成 `run_storage_gc_at_boundary` → `context-simple/store.rs` 的删除计划没有 checkpoint recovery roots。探针：protected reconcile 正确保留 blob，随后普通 Storage GC 删 1 个 blob，恢复 A 后其 Live 正文 fetch=None。补充静态风险：根枚举把 list 错误变空集、load 错误跳过。**已落地：① 契约新增 `storage_gc_protecting(roots, roots_complete)`（默认只在根集完整且为空时放行普通 pass，否则延期删除）；context-simple 实现把保护根并入强引用根集（含依赖闭包走查），`roots_complete=false` 时本 pass 零删除并在报告 reasons 写明延期。② `collect_checkpoint_recovery_roots` 返回 `(roots, complete)`——list 失败/行数触顶/单条 load 或 decode 失败都置 incomplete，不再把读失败包装成空集；同一信号同时约束 `reconcile_store_protecting`（签名加 `roots_complete`，incomplete 时 stale-duplicate 删除分支延期、报告注明）。③ 任务完成边界 `run_storage_gc_at_boundary` 经 `context_storage_gc_protecting` 传入保留根。回归（引擎级完整序列）：保护根 blob 在完成边界 GC 存活、incomplete 全延期且报告可见、无保留根对照照常删除；R03 引擎级探针与 host_restore 3/3 保持绿** | `Delete ∩ Reach_strong(CurrentRoots ∪ RetainedCheckpointRoots) = ∅` 约束每个物理删除入口（Storage GC 与 reconcile 删除分支共用根集合）；根读取失败时携带「根集合是否完整」、未知暂缓删除 | 不复活终态语义；不另建删除通道 |
| **W04（P1，已落地 2026-09-10）** | `context-baselines/rolling.rs` 一次 maintain 循环到阈值满足（默认配置＋200,000 字符旧输入→**132 次串行 compactor 调用**，脚本计数）；`agent-compose/src/compactor.rs` 直接 await 模型＋与操作无关的 CancellationToken；`actor/model.rs` 在 Actor 命令/完成处理分支内 await maintain，`actor/mod.rs` 直到分支返回才处理命令——维护已进入等待后 cancel 250ms 无回执。**已落地：① 预算——`RollingConfig.max_compactor_calls_per_maintain`（默认 4）：一次维护最多串行 N 次压缩器调用（单次输入/输出已有硬上限，pass 总开销有界）；超预算保留未消费残余并如实报告 `ContextMaintenanceReport.deferred_folds`（serde default 兼容），下一次维护继续消费；回归验证预算内调用、延期可见、重复维护收敛。② 取消身份——BeforeModel 维护改为 spawned operation（`OpKind::Maintenance`＋`InFlightOp.abort` 句柄，与模型调用同一 op 模式）：actor 循环在维护期间继续处理命令，`cancel_turn` 既有路径先 cancel token 再 abort future（丢弃 future 是引擎文档化的安全失败——FoldRestore 守卫归还全部移出记录），turn 以 TurnCancelled 终结；维护完成经 OperationCompletion 回 actor，代际校验通过后恢复回合准备（`continue_model_operation_after_maintenance`），stale 完成照常丢弃。回归：门控引擎阻塞维护时 cancel_turn 2s 内拿到类型化回执、被门控 future 真实 abort、模型零调用、无 ModelStarted；既有 turn 120＋lib 全绿证明完成路径无回归** | 保留 Actor 唯一编排，维护有明确调用数/字符量/时间预算与取消身份；超预算保留未处理残余并报告延期；必要长等待经既有异步操作完成消息交回 Actor、代次校验后提交。验收覆盖「维护已开始时取消」与主决策之外的总模型调用预算 | 不新增并行 worker 或通用调度器；不用调用完成后的统计充当调用开始前的预算准入 |
| **W05（P2，已落地 2026-09-10）** | 契约允许 16 个 verification coverage domains、32 条 acceptance criteria；ExecutionState 只保留最近 8 个 VerificationFact，`task.rs` 要求验收 receipt 引用的 PASS 仍在数组。探针：同 basis 9 域可信 PASS 后 validity=Current 但只有域 1–8 可查询，补跑域 0 又挤掉域 1——`|required|=9 > |retained|=8` 恒 uncovered。**已落地：① `MAX_VERIFICATION_FACTS` 对齐契约 `MAX_VERIFICATION_COVERAGE_DECLARATIONS`（16）；② `cap` 的淘汰策略改为「每个不同 verification_identity 保留最新一条，其余名额按新旧补齐，同一身份至多保留一条」——同域重复验证在原地刷新，不再把其他域的唯一证明挤出去；legacy 空 identity 行按新旧照旧淘汰。回归：9 域全保留、重复域 0 至溢出后 9 域身份各留最新一条、spec 变更仍按规则整体失效（既有失效测试保持绿）** | 当前验收所依赖的证明与一般历史尾部区分保留（同域重复 PASS 不挤掉其他域），仍由现有 ExecutionState/TaskAnchor 管理 | 仅把 8 调大继续按调用次数淘汰不解决；不给默认 OperatorClosureOnly 新完成权 |
| **W06（P2，已落地 2026-09-10）** | 进程日志允许捕获 8 MiB（`tool-runtime/src/tools/stream.rs`），但 `artifact.read` 在使用 start/end line 前先读 2 MiB+1 并拒绝；错误建议「use a narrower range」，缩小范围不改变判断。探针：合法 3,000,000 字节工件请求前 200 行/第 1 行均被同一大小检查拒绝。**已落地：读取改为流式按行扫描——`take(8 MiB)` 扫描预算（与生产者捕获上限一致，合法工件全可达）＋`read_until` 逐行捕获请求窗口（捕获上限 2 MiB，超出诚实截断），预读全文拒绝路径删除；分页元数据保留并新增 `total_lines_complete`/`window_truncated`，扫描预算停住时 `has_more` 保守为真、summary 注明预算与不完整原因。回归：3,000,000 字节工件首页可读、第 25,000 行深区间可读且游标准确、既有分页/越权/范围测试全绿** | 合法产出的工件要有受预算、有限步的按范围读取，或实际可达的 chunk/tail/search 操作，返回准确游标与不完整原因；拒绝建议必须有改变结果的可能；保留每次 IO/输出预算 | 不直接无限读取；不增大全文上限 |
| **W07（P2，已落地 2026-09-10）** | `tools/patch.rs` 顺序在临时 updated 上应用 hunk；后续 hunk 失败时把 updated＋磁盘旧 revision 传入 `patch_refusal`，渲染成 current revision 纠错候选却未声明是部分 patch 的假设结果。探针：候选包含第一 hunk 写入的 NEVER_COMMITTED_VALUE 并带旧 revision，磁盘完全未变。**已落地：失败 hunk 的纠错候选改用磁盘原文 `original`（磁盘未变，updated 是未提交的假设中间态），拒绝消息带失败 hunk 序号（`hunk 1`）。回归：第一 hunk 改写＋第二 hunk 失败 → 候选不含 NEVER_COMMITTED_VALUE、含失败 hunk 序号、磁盘逐字节未变** | `current_candidate(revision) ⊆ actual_content(revision)`；保留中间 hunk 诊断须明确 hypothetical＋失败 hunk index，并另给真实磁盘基准；默认纠错候选取 original | 不让 Agent 据候选生成引用磁盘上不存在文本的下一次 patch |
| **W08（P2，已落地 2026-09-10）** | Rolling 在 BoundedCompactor 返回 Err 时归还记录；但真实 `ModelBackedCompactor` 把任意模型错误改成 `Ok(fallback)`（512 字符前缀），Rolling 按成功提交并移除原记录。探针：模型调用失败但 maintain 报 archived=1，随后 engine checkpoint 不再含原唯一约束（位于 700 字符之后）。**已落地：适配器删除 fallback 路径——模型错误原样传播为 Err（summary_unavailable），空回复同样返回 Err（调用成功但无可用摘要不得拿源前缀冒充折叠结果）；Rolling 既有失败守卫归还记录、旧摘要不触碰。回归：适配器错误传播＋空回复拒绝；引擎级探针（512 字符外尾部约束＋必败模型）maintain archived=0、约束仍在工作集、无 Summary 铸造；agent-eval CI 用 ScriptedCompactor 不受影响** | 区分 `summary_completed` 与 `summary_unavailable`；临时错误、取消和未知结果交回引擎保留/延后消费——显示用 fallback 非空不获得退役源正文的资格；与 W04 维护取消落地同时守住失败语义 | 未声称全系统永久删除用户正文（原始历史另有工件保存）；不因 fallback 可显示而提交退役 |

## 2026-09-07 续审残余（基线 `b299c6a` → M17）

审查为部分源码静态续审（19 路径、4 完整返回；无本地工具链），非全仓逐行通过结论。F01–F09 关键代码定位已于 2026-09-07 在本工作树 HEAD `7c3236d` 静态复核成立；均为条件性风险，未做真实 PID 复用、故障注入或 GUI 实测。工单全文见 [reviews/2026-09-07-platform-native-audit/NEXT_STAGE_TASKS.md](reviews/2026-09-07-platform-native-audit/NEXT_STAGE_TASKS.md)。

| 发现 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **F01（→B1，主体已关闭 2026-09-07）** | `tool-runtime/src/supervision.rs`：台账行仅 `{"pid","purpose"}`；`reconcile_children` 用 `process_is_running(pid)` 后 `kill_process_tree(pid)`。`agent-process/src/lifecycle.rs` 已有 `ProcessIdentity`/boot_id+starttime/创建时间核对未复用 | 记录稳定创建身份；无法确认身份不得发 kill；遗留纯 PID 记录不自动当可信对象 | 不为测试制造真实 PID 复用误杀；不新建第二套 Supervisor 框架 |
| **F02（→B1，主体已关闭 2026-09-07）** | 同上：`record_child` 写失败被忽略、无耐久同步；读失败返回空集合、坏行跳过；kill 后直接入 `killed` 并删台账；`ChildLease::Drop` 无条件移除 | 台账 IO 返回 `Result`、有界、串行、必要耐久；区分"发出清理/确认退出/允许复用工作区"；未确认记录保留、错误如实返回 | 不重写日志系统；`Workspace::open` 的独占日志启动互斥是另一层，不据此泛化并发清理结论 |
| **F03（→B1，已关闭 2026-09-07）** | `tool-runtime/src/proof_runner.rs:54` 自建 `ProcessRunTool::new`（`tools/process.rs:623` 默认 `host_death_watchdog=false`）；`registry.rs` 仅给普通 dispatcher 开启，compose 装 proof runner 未贯通 | 监督配置由 Rust 宿主统一注入普通工具与宿主验证两条车道 | 不据此否定 Windows Job 路径；不为宿主验证伪造 Core effect 身份 |
| **F04（→B1，已关闭 2026-09-07）** | `agent-process/src/watchdog.rs:67` 以 `kill(leader,0)` 判活（OS 探针证：组长被 reap、同组成员仍活时判定 false）；`:88-94` `Drop` 同步 `child.wait()` 无期限 | 区分通用命令的后台后代与宿主验证的整组受控；`Drop` 等待加期限 | 不简单删判活条件后按旧 PGID 无条件发信号；不让任意 .NET 可执行文件被动承担 re-exec 协议 |
| **F05（→B2，已关闭 2026-09-07）** | `agent-storage/src/lib.rs:314` `persist_authority_metadata`：temp 写+sync → rename 发布 → `sync_directory(parent)?` 失败仍返回 Err；`compact_locked`（`:700`）在内存换 writer 前被 `?` 中断 | 错误携带"可能已发布"阶段或等价围栏；显式 `compact_authority_journal` 同样受控 | 不删目录同步换测试绿；普通 append 路径已有围栏不拆 |
| **F06（→P2）** | `agent-runtime/src/status.rs:104` `anchor_revision` 跨任务 `max` 且 `FocusChanged` 不重置；`agent-tui/src/cli.rs:301` 以输出文字含 `denied by approval policy` 判审批拒绝；终态区分不足 | 平台输出中立类型化状态：执行阶段/任务生命周期/完成来源/审批结果/连接完整性分开 | GUI 不解析 `lines()`/summary/工具正文构造权威结论；缺口标 partial/resync 而非 stderr 警告后照常输出 |
| **F07（→P1）** | `agent-tui/src/work.rs:18-50`：set_focus→list_tasks→replace→user_message 多次独立 await，`UserMessage` 不绑定任务身份 | 绑定任务与预期版本的提交或原子工作入口；复用 TaskManager 事务 | 不在 GUI 端加锁了事（另一入口不遵守）；幂等回执不靠客户端超时换 ID 重发 |
| **F08（→B3，主体已关闭 2026-09-07）** | `context-simple/src/gc/reachability.rs:110` `queue_error_verifications` 以输出实体匹配所有 live Error | 可信验证事实携带故障/任务/覆盖域/资源版本关联；关联不足只提升相关性或成候选 | 不授予不可逆 `VerifiedFixed`；Context 错误终结≠Runtime 完成 gate，不混写 |
| **F09（→B3，主体已关闭 2026-09-07）** | `agent-tui/src/cli.rs` `resolve_prompt`（`--prompt=-` 全量 `read_to_string`）；grant 文件 stat 后整体读取；headless 同步写 stdout/文件不受事件等待超时约束 | 接入时限制字节与解码成本；慢消费者有界队列/期限/断线重同步；大正文传引用按需读取 | 不只换编码格式保留无界数据流 |
| F10（→C0，文档） | NEXT_TASKS 旧声明"无工程开放项"范围过宽 | 已由 2026-09-07 文档切换改为具体已验范围 | 不把旧报告全部翻成未完成，不重开 M16 |

**B 系列关闭记录（2026-09-07）：** B1/B2/B3 的主体修复已在工作树落地并验证——B1 身份化台账＋类型化对账门＋清理回执＋proof 监督接线＋watchdog 组成员扫描/有界 Drop（Windows 与真 Linux 双侧定向回归；后续加固演化与扩展验收见 [reviews/2026-09-07-worktree-review/](reviews/2026-09-07-worktree-review/REVIEW.md)，P3 宿主接线、spawn 窗口、冷恢复孤儿随 P3 收口）；B2 发布不确定 `RecoveryRequired`＋compact writer 围栏（agent-storage 22，注入测试稳定）；B3 同配方关联终结＋stdin/grant 有界读＋有界输出 sink（context-simple 288、agent-tui cli 16）。B3 剩余：recipe 版本/覆盖身份与任务级关联、无期限 stdin 读取期限。

续审同时确认**不再原样重报**的旧问题（代码已变）：release 消费 ACK stamp、process.run 输出 EOF、Windows metadata 替换不先 unlink、普通读取不再清错误、片段 supersession 覆盖判断——见 REPORT.md 第 4 节。

## 仍开放（条件项）

| 工单 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| PACKAGE-01 | `dist.sh` / `dist.ps1` 接受 target 但不传 `--target-dir`；复用 `dist/<version>`。Bash 桩测：旧产物可被成功打包 | 下次实际发布：构建输出与复制源同一身份；干净 staging；PowerShell 原生退出码（R1 一并收口） | 不宣称当前已发布 ZIP 已错 |
| MCP-01 | 写请求阶段只有 deadline，取消在读阶段 | 仅当默认产品启用 MCP 写路径时（E1 若声明支持某 MCP 路径亦触发）：写/连接/读都可取消；半帧毒化 session；await reap | 不启用第二调度器；不以"默认未开启"永久豁免显式使用 |

PROCESS-01 的已关闭部分（Windows Job 围栏、Unix 管道 EOF 看门狗、台账本体、真 Linux 验证）保持关闭；其新增残余（台账身份与清理确认、proof 接线、watchdog 边界）即上表 F01–F04，归 B1，不重立 PROCESS-01。

## 本分支已关闭，不重复立项

| 项目 | 处理 |
|---|---|
| STORAGE-02 | 已落地（`f9852ea`）：`compact_locked` 全部可失败步骤前移到 metadata 发布点之前，发布后仅内存 writer 交换与尽力删除旧 WAL；agent-storage 21/21 |
| PROCESS-02 | 已落地（`7c72df3`）：`reap` 仅在确认退出后清 pid（类型化 `ProcessReapOutcome`）；`kill_tree` fallback 无锁 direct kill；agent-process 30 |
| WORKSPACE-01 | 已落地（`17c5ded`）：普通 open 带 `O_NONBLOCK`；`project_markers` 仅元数据探测（`fstatat NOFOLLOW`）；FIFO watchdog 回归；agent-workspace 98+5+3 |
| WORKSPACE-02 | 已落地（`17c5ded`）：六处 raw HANDLE 先 `from_raw_handle` 接管再 reparse 检查；句柄计数故障注入未做 |
| PROVIDER-01 | 已落地（`3e0128a`）：非 2xx 走 `bounded_error_body`（8 KiB 上限 + 每块 deadline，截断显式标注）；provider-openai 107 |
| PROVIDER-02 | 已落地（`3e0128a`）：Chat `length` 映射 `ModelOutputLimit`，不把截断前缀重放为正常完成 |
| PROVIDER-03 | 已落地（`3e0128a`）：EOF 尾帧过 `validate_sse_event_routing`，矛盾帧拒绝 |
| CONTEXT-01 | 已落地（`3e0128a`）：依赖扫描 newest-first，桶内删除改序保持（`remove` 不 `swap_remove`）；context-simple 287 |
| STORAGE-01 | 已落地：Windows `MoveFileEx` 替换 metadata，不再先删；缺 metadata 且有 WAL 代际则 `RecoveryRequired`，不铸空 g1、不选最大 `.gN`。`cargo test -p agent-storage` 覆盖残留代际与覆盖写。Windows 在替换中途杀进程仍未注入 |
| DOC-01 | 活动 CURRENT 与已落地文件的矛盾已随 D0/M16-00 关闭。检查脚本仍只验结构/链接，不是全部状态断言的语义一致性 |
| 输出 EOF 后在 `select!` 外 `child.wait()` | F3-6a：`outputs_closed` 后继续守超时/取消 |
| 消费 ACK 在 `debug_assert!` 内 | F1 / `c6fbbab` |
| PromptRequired 可进入普通候选第二次 | F1 / `c6fbbab` |
| TUI resync / run_summary / shadow 去重 | F4 续审批次 |

审查重读 `12c8628` 仍会看到旧 wait/ACK 路径，不能用来否定本分支修复。

## 已并入产品切片、不是当前执行项

| 缺口 | 归属 | 状态 |
|---|---|---|
| TUI 没有 continue、忙时输入被丢弃 | M16-01 / F1 | 代码已落地；TUI 走查待做 |
| 可取消验证仅实验组合启用 | M16-03 / F3 | 取消桥接已落地；`--defer-proof` 改默认仍等慢验证走查 |
| 文件版本身份 / 清错 / grep PARTIAL / 有界读 | M16-06 / F5 | 已落地 2026-09-06 |
| Resident/Warm 对 lease/TTL 保护的处理差异 | 仅有用例触及时 | 未动；不开始通用 GC 改造 |
| 无 Cargo.toml 时空 recipe 表启动失败 | M16-08 / F6 | 已落地 |
| 无头 live 用了 permissive 审批 | M16-07 / N1 | 产品 CLI 禁止 `--yes` / `--allow-all` |
| 编辑器任务难在 argv 嵌 grant JSON | M16-07 / N2 | `--grant-file` + `--jsonl-out`；无 daemon |

## 什么可以打断主线

当前默认产品路径上已证实的权限绕过、数据破坏、重复副作用、不可恢复错误或直接阻塞本工单的崩溃。
先定位最小触发条件，修复并保留必要回归。无法可靠处理时停用受影响路径、明确限制，不假报安全，也不绕过 Core。

## Backlog（live 走查发现，2026-09-07，当前 HEAD `662e952` 后）

| 现象 | 复现 | 影响 | 处理 |
|---|---|---|---|
| 多文件 `edit.patch` 的写集合要求单个 standing grant 前缀覆盖全部目标；按文件分别授权时批量 patch 永远被拒（同路径单文件 `edit.replace` 可过） | 真二进制 live：两个分文件 grant + 跨两文件的 edit.patch → `tool denied by approval policy`（`agent-core/src/approval.rs` `grant_matches` 的 `WorkspaceWriteSet` 分支） | 可用性限制，方向 fail-closed，无权限扩大 | 有意保守设计，维持；需要时给操作者「组合 grant/公共前缀」的使用指引，或多 grant 交集匹配需单独设计评审 |
| 恢复会话的无头 `session_end.task_state` 报 `none`，尽管 restore 后有活动任务并完成了 continue | `--restore=latest --continue` 后看 JSONL 末行（Drain 只统计本进程 live 事件） | 低：少报不虚报；脚本侧待审阅语义在恢复会话失真 | backlog；修法是让 restore 回放也驱动 Drain 的 task_active |

## 什么不自动打断主线

实验 sidecar、未启用平台能力、未出现的规模边界、性能猜想、旧窗口统计、通用化需求。
保留到历史/候选池，由实际使用或明确研究任务重新选择。MCP-01 在产品未启用 MCP 时属此类。

每条新记录只需：现象、当前 SHA、复现、影响哪个功能、处理或延期理由。
缺陷数和测试数不是交付进度；不要为每个发现新造一组里程碑。
