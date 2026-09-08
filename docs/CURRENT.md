# 当前工作

## 现在做什么

**当前阶段：M17 收尾——可恢复的多入口工作台（N 系列，2026-09-08 切换）。** M17 三线的代码主体已落地（B1–B3、C0、P1、P2 主体、P3 宿主、G1–G3 客户端侧、E1；对照见 [NEXT_TASKS.md](NEXT_TASKS.md) 的 M17 队列表）。本轮的判断变了：**主要缺口不是缺组件，而是端到端链路断着**——订阅成功但事件 receiver 被丢弃、重连自动重发修改操作、宿主恢复绕过正式信封解码、多连接/会话释放/停机缺口。2026-09-08 外部闭环审查（基线 `11afdd747d6cbbb58ef0d7371841e8e365c4f8db`）的 20 项发现（F01–F20）映射为 N0–N8。

**N0 已关闭（2026-09-08）：CI run `34163939549` 七 job 全绿**——fmt 修复、宿主 Linux cfg＋UDS e2e、.NET job 入 CI、F20 测试修准；修复链顺带消掉了 conformance 角色准入、protocol 夹具 lint、agent-replay 探针夹具（契约演进被 fmt 红遮蔽四轮的实证）、supervision 锁退避（两进程同 workspace 启动的真实生产问题）。第一批 **N1/N2/N6 已关闭并合入 main**（CI run `34173331100` 七 job 全绿）：宿主长期服务、客户端操作安全、核心语义边界三线由并行代理实现、主会话按 A→B→C 集成验证（host e2e 双平台 / dotnet 35 / context-simple 302 / workspace 全量零失败）。第二批 **N3/N5 已关闭并合入 main**（CI run `34268863699` 七 job 全绿）：真实事件从 Runtime 经宿主到客户端全链贯通、多行输入合法、正式信封恢复（N5 的结果/差异/工件读取半转入后续）。**N4 已关闭（2026-09-09）：CI run `34278810636` 七 job 全绿确认**——知情审批快照＋桌面稳定行生命周期＋真实事件后台消费＋真实宿主默认传输（`9fb2030`/`433d21e`/`843803f`；dotnet 56/56）；随后按原顺序 N7 长会话、N8 能力配置与来源绑定发布（含 PACKAGE-01）。**N7 已关闭（2026-09-09，`87850ae`）**——MetricsSession 覆盖标注 root_only/full_tree/unknown＋有界采样环＋显式 idle 标记＋DeltaCoalescer 定时刷新（C4 记录）。**N8 已落地大半（2026-09-09）**——PACKAGE-01 来源绑定打包（`3352273`：--target-dir 同身份、干净 staging、agent-host/desktop 入包、SOURCE.txt、递归 SHA256SUMS、PowerShell 原生退出码；Windows 端到端验证）＋.NET→宿主→Runtime→工具→事件→GUI 全链 e2e（`aeddfbd` HostChainTests，真实宿主二进制＋demo model，本机 1/1）；剩余 B4 宿主受限 MCP/Plugin 配置路径由 B 线代理并行执行中。并行线同步落地：B2（`fd86e2f`）、A2/A3（`00c253c`）。2026-09-09 外部三线审查（基线 `bbf7f5d`）开出并行三线 A/B/C，与 N 系列同步执行、互不干扰（见下节）。

审查原文：[reviews/2026-09-08-closure-audit-11afdd7/REPORT.md](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)；
工单全文：[reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md](reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md)。
执行队列：[NEXT_TASKS.md](NEXT_TASKS.md)。

## 2026-09-08 闭环审查（F01–F20 → N 系列）

审查为部分源码静态审查（55 路径正文；apps/Agent.Desktop 10 文件、clients/dotnet 19 文件、agent-host 5 文件全文；克隆仍因 DNS 失败，无本地工具链，未运行构建/测试）。F 系列关键定位（subscribe 丢弃 receiver、FIRST_PIPE_INSTANCE、无条件 remove_file、会话不 revoke、裸 JSON 恢复、validate_text 拒换行、AsyncCommandGroup 积累、Resident/Warm lease 差异、to_summaries 全量投影）已于 2026-09-08 在本工作树 HEAD `11afdd7` 静态复核成立。逐条细节与不要做什么见 [AUDIT_TODO.md](AUDIT_TODO.md)。

**先确认已修好的旧问题（不原地重做）**：监督台账身份/类型化对账/确认式清理；proof runner 监督接线；metadata 发布不确定围栏；原子 `StartWork`＋进程内受理台账；同任务同 `VerificationProbe` 验证关联；`skill_read`＋MCP/Plugin 配置缝。均为定向静态确认，不升级为全平台运行验收。

**链路断点按 N 归属**：事件转发断（F06/F11→N3）；宿主长期服务断（F02–F05→N1）；修改重发与连接终态（F07–F09/F19→N2）；正式信封恢复（F10→N5）；知情审批与对象生命周期（F12/F13→N4/N7）；决策误终结/lease 跨层/Skill 句柄/catalog 投影（F15–F18→N6）；测试名与路径不符（F20→N0/N8）。

上一轮（2026-09-07，基线 `b299c6a`）的 F01–F09 残余表已由 M17 工单消化：B1/B2 关闭，B3/P1/P2 主体关闭（recipe 版本/覆盖身份关联、无期限 stdin 读取期限两项残余随 N6/N3 收口）。原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)。

## 2026-09-09 三线审查（基线 `bbf7f5d` → 并行三线 A/B/C）

外部审查（基线 `bbf7f5d3080747fe113a4a07469ac2fd4ccf2d35`，即 N3/N5 关闭的 docs 提交），原文：[reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)。审查性质同前：无完整 checkout、无 Rust/.NET 工具链、未运行仓库构建/测试；20 个 crate 目录树核对、28 路径重点阅读（14 全文）；隔离 Python 探针不是回归测试。结论逐项处置，不构成全仓冻结。

**确认不重做：**事件 receiver 保留、查询/修改重试分离、正式信封恢复入口、Resident/Warm 到期保护、Skill 受限文件打开——上轮修复保持。新问题集中在：① A 线——Rolling 第二次折叠不把旧摘要放进压缩输入（本工作树已复核：`rolling.rs` `take_fold_job` 的 prior 只收集本次移出记录）；Context 正文截断后仍可能声称覆盖原始行范围（partial/required 传播同类）；完整 GC recall 后删 blob 发生在新持久状态之前；GC 外置 future 被取消不进回退；隔离失败仍移除 owner；admit 终态条目先迁移后拒绝、老 Ephemeral 准入无新使用期。② B 线——快照与订阅切点不一致会漏中间段、live-only delta 与 durable watermark 过滤冲突；`BoundedEventQueue` 的 `Completion` 创建即完成（本工作树已复核 `BoundedEventQueue.cs:54`）；宿主 `cancel_all` 清表先于取消、`wait_empty` 用已清空表判断结束；`retyped` 验证前清空 `work` 字段。③ C 线发现（事件消费、审批详情、稳定行）已在基线后四提交落地，随 N4 验收，不重复立项。

**执行方式（2026-09-09 起）：N 系列主顺序不变（N4 收尾验收 → N7 → N8），三线 A/B/C 与之并行、互不干扰**——A 拥有 `context-simple`/`context-baselines`，B 拥有宿主/协议/.NET 客户端库与共享 DTO（单一合入），C 拥有 Avalonia 界面；重叠切片一次执行、两边同时关闭。切片表与所有权规则见 [NEXT_TASKS.md](NEXT_TASKS.md)「并行三线」节；缺陷明细见 [AUDIT_TODO.md](AUDIT_TODO.md) 2026-09-09 表。

**B1 已落地并关闭（2026-09-09，三线首切片）**：SNAP-GAP——`ResumableSession` 握手改为 subscribe→snapshot 次序（宿主订阅的 receiver 注册先于其快照屏障，此后事件不可能落在流与状态读取之间），事件泵按快照 watermark 对 durable 通知去重，重连安装即原子清空会话级事件队列（有序复位边界，旧连接未读事件不跨代存活）；LIVE-DELTA——契约新增 `RuntimeEvent::is_live_only`（`ModelDelta`/`ModelRetrying`），宿主转发器按 kind 分流：live-only 恒转发（其内容不入任何快照，游标重复不再被丢），durable 仍按切点去重，host e2e `assert_notification` 同步修准。无 wire 变更、不改 DTO 形状；`BoundedEventQueue.Clear()` 为新增方法（Completion 语义属 B2 的 QUEUE-COMPLETION，未动）。实际检查：`cargo fmt -p agent-host -p agent-contracts -- --check`、`cargo test -p agent-contracts --lib` 163、`cargo test -p agent-host`（含 named-pipe e2e 7/7；stop_bounded 首跑负载抖动、单独与整包重跑均绿）、`dotnet test clients/dotnet/Agent.Client.Tests` 全绿（62 项，含本切片新增切点去重/重连复位/队列 Clear 三测与既有 56 项；计数含并行线当日新增测试）、`dotnet build apps/Agent.Desktop` 0 错误。未验证：真实 provider 与多客户端交错场景照旧 NOT_RUN；下一步 B2（QUEUE-COMPLETION / CANCEL-ALL / RETYPED）。

**B2 已落地（2026-09-09）：**QUEUE-COMPLETION——`BoundedEventQueue` 的 `Reader.Completion` 从构造起保持同一未完成 TCS，`TryComplete` 后仅当 backlog 排空才结算（最后一次 `TryRead` 触发；空队列关闭立即结算；错误延迟到排空后 fault；`Clear` 视为排空等价），对齐 `ChannelReader<T>.Completion` 语义；CANCEL-ALL——`LiveStreams::cancel_all` 只发中断不清登记表（worker 自删表项；hook 在表锁外调用），`drain` 第二段 `wait_empty` 因此观察真实 worker 退栈、超时留可见残余；main.rs 以 select 等待「Ctrl-C 或 serve 线程先退出」使提前失败收敛，`composed.shutdown()` 结果捕获、传输停止（stop＋poke＋join）不再被失败跳过；RETYPED——`retyped` 原样携带 `request.work` 交 router 既有验证器，携带 work 的 run-scoped 请求收到结构化 `protocol.request_invalid` 拒绝而非被清洗受理。实际检查（提交前在含并行三线未提交改动的共享树上重跑）：B2 单测 3 项全绿、named-pipe e2e 8/8（含新增 retyped 拒绝测与 hostile-clients 停机测——后者现在真实等待 worker 退出与 grant 释放）、host_restore 3/3；`dotnet test clients/dotnet/Agent.Client.Tests` 72/72（含本切片新增 5 项 Completion 回归，计数含并行线当日新增测试）；本切片 `lib.rs` 经 rustfmt（edition 2024＋仓库 rustfmt.toml）复核通过——共享树 `cargo fmt -p agent-host -- --check` 的红来自并行 N8 线未提交的 `config.rs` 格式漂移，不属本切片内容。未验证：Linux UDS 侧 e2e 与 CI 全量照旧待 run 记录；真实 Ctrl-C 信号路径未实测（msys 无法投递，沿用既有 stop/wake 模拟）。下一步 B3（GUI 所需真实任务/审批详情/结果/工件/只读 Context 接口，即 N5 结果半）。

**B3 已落地（2026-09-09）：**4 个 run-scoped 只读路由，交付「GUI 所需真实任务/审批详情/结果/工件/只读 Context 接口」（N5 结果半，C3 读依赖）——`work.task_detail`（单任务完整 anchor 卡：plan/acceptance/open-loops，Runtime 新增只读 `TaskDetail` 命令走 actor 单一序列化点）；`work.changes`（workspace 变更日志有界尾读：`Workspace::read_changes` 顺序读＋环形缓冲 O(limit) 内存、exclusive `after_tx` 游标、同锁与写者互斥、坏行 fail closed；协议镜像 `ChangeSummary` 六变体，`old_content` 内部捕获不落 wire）；`work.artifact`（`open_artifact_for_run` 认证 run 归属＋sealed digest 校验后按预算有界读，base64 传输，`truncated` 诚实标记，size_bytes 为磁盘真值）；`work.context`（`inspect_context` 有界摘要，limit 钳到 512）。授权：`WorkControlGrant` 增 4 只读动作，read_only 与 operator 会话都允许（观察类）；协议：DTO＋8 验证函数＋4 Route 方法，无 wire 变更到既有路由；宿主：`HostPlane.workspace` 接线、dispatch 4 分支；.NET：镜像 DTO（ChangeSummary 严格 converter 拒绝 `old_content`）、`AgentConnection`/`ResumableSession` 4 方法、fixture/接口同步。实际检查：`cargo test -p agent-platform-protocol --lib` 42/42（含 5 项 B3 单测）、`cargo test -p agent-workspace --lib` 105/105（含 read_changes 5 项）、`cargo test -p agent-runtime --test actor` 72/72（work_control 含新 router 签名）、`cargo test -p agent-host`（lib 5/5、named-pipe e2e 8/8 含 B3 4 条断言流、host_restore 3/3；e2e 整包一次绿，`stop_is_bounded_with_no_client` 首跑偶发连不上、单独/整包重跑均绿——B1 已记载同类负载抖动）、`dotnet test clients/dotnet/Agent.Client.Tests` 77/77（含 4 项 B3 conformance）；B 线 crate `cargo fmt -- --check` 通过（并行线未提交已清空，工作树只含本切片改动）。未验证：Linux UDS 侧 e2e 与 CI 全量待 run 记录；GUI 消费端接线属 C 线（C3 剩余项），B3 只交付接口。下一步 B4（同一正式宿主 profile 多入口复用、已有 MCP/Skill 配置接入，即 N8）。

**A1–A3 已落地（2026-09-09，A 线首批切片）：**A1（`6f47a90` 修复＋`df5b972` 测试对齐，已随 N4 批次合入）——GC-DEL：`commit_full_gc` 不再返回删除清单，Context GC 只外置（Cold→External 引用），物理删除唯一经 Storage GC 按保留与引用规则执行，recall 后 blob 由 startup reconcile 对着实际恢复的 state 收回；GC-CANCEL：外置条目从不离开 `pending_externalize_retry`（plan 只携带 id＋锁内预序列化字节，丢弃 IO 阶段至多丢写入不丢条目，temp+rename 原子写）；QUARANTINE：隔离返回明确结果，rename 失败保留 owner 与完整性拒绝；ADMIT-TERMINAL：liveness 判定前移到 `plan_admit`，拒绝先于任何迁移；ADMIT-TEST：admit 并发测试改确定性屏障 `IoBoundaryPause`（停泊外部读边界，diagnostics 停泊期间完成即证明无持锁，替代相对耗时推断）。A2（本批提交）——RANGE-PARTIAL：同 revision 覆盖证明拒绝裁剪正文（声明行区间是工具报告区间，不是保留覆盖证明）；selected/foreground 的 `partial_body` 如实反映摄取期裁剪（descriptor 是身份卡永不 partial）；required 的可见副本仅当非 partial 或恰好暴露 overlay 会嵌入内容时满足 claim。A3（本批提交）——ROLLING-PRIOR：Rolling 折叠输入 = 旧摘要＋本次移出记录（有界），`FoldRestore` 守卫在失败/丢弃时还回记录，压缩失败不覆盖旧摘要；ROLLING-FOCUS：Rolling 跟踪 focus（diagnostics 暴露 `focus_task_id`/`focus_generation`，checkpoint/restore 携带），`host_restore` 集成改用默认 Rolling profile 验证活动任务检查点恢复；ADMIT-LEASE：准入不再刷新创建时钟，改授有界使用期租约（`state.turn + max_lease_turns`，与 `context.lease` 同一上限，更长未到期租约不缩短），residency 两条终结路径共用 `protected_from_expiry`。实际检查：`cargo test -p context-simple` 311/311（含 A2 新增三项与 ADMIT-LEASE 回归）、`cargo test -p context-baselines` 11/11（含 ROLLING-PRIOR/FOCUS 回归）、`cargo fmt -p agent-host -p context-simple -p context-baselines -- --check` 通过（共享树 fmt 红确认来自 N8 线未提交 `config.rs`，A 提交树复核通过）、`cargo test -p agent-host --test host_restore` 在暂存 B 线改动后的纯 A 提交树 3/3。未验证：崩溃恢复路径未随 GC-DEL 重跑；RANGE-PARTIAL 对完成门禁的影响未做端到端回归；正式冷恢复支持声明仍等 N5 验收。A 线剩余 A4（报告第七节设计建议，非缺陷）按真实瓶颈另行安排。

**C 系列已落地（2026-09-09）：**C1/C2 随 N4 关闭（`9fb2030`/`433d21e`/`843803f`）；C3 走查 `391f121`——未知提交按快照事实解除（host 重启退休 client_request_id，同名任务可见即受理成立、不可见则再次提交是新任务，绝不自动重发）＋restore-walkthrough 走查测试（scripted host 上的连接丢失/重启/重建）；C4 的 N7 `87850ae`（MetricsSession 覆盖标注 root_only/full_tree/unknown、有界采样环、显式 idle 标记、时间预算 coalescer flush）＋N8 打包 `3352273`（dist.sh/dist.ps1 来源绑定：`--target-dir` 构建入同一 staging 目录防陈旧混入、干净 staging、agent-host＋desktop publish 入包、SOURCE.txt 来源身份、递归 SHA256SUMS、PowerShell 原生退出码——`Invoke-Native` 修复 `${LASTEXITCODE}` 解析）。实际检查：`dotnet test` 72/72（核心，HostChainTests 另跑）、dist.ps1 Windows 端到端打包成功（agent-tui/agent-host/desktop＋SOURCE＋SHA256SUMS 全就位）、bash 语法复核。未验证：修改审阅/工件按需读取/只读 Context 接线依赖 B3 结果/工件读取接口（B 线在途 config.rs/HostChainTests）；完整 dist 的 Linux 侧由 CI package job 复核。

**CI 事实：** run `34271105841`（`bbf7f5d`）Windows 全测试失败于 `context-simple` admit 并发测试（其余 job 通过）；确定性屏障已随 `6f47a90` 落地（替代 `7e026ee` 的比例断言）；N4 批次 run `34278244036`/`34278810636` 已全绿确认。CI 结论按 run 记录，不外推到任意 SHA。

## 执行原则（M17 收尾）

- GUI 的布局、客户端库、只读状态与任务输入可与基础修复同时开始（G1 等 C0）。
- 涉及宿主执行、可靠清理与冷恢复的**正式支持声明**，等待 B1/B2 对应验收（G2 依赖如此）。
- 无关的研究性优化（评分、缓存、GC 调参）不阻塞平台与 GUI，也不在本阶段默认开启。
- G 线已落地事实（2026-09-07）：G1（.NET 客户端＋Avalonia 外壳＋双语 C0 fixtures 一致性）、P3（`crates/agent-host` 命名管道宿主＋互操作冒烟）；G2/G3 的客户端与宿主链路落地，差异审阅与 Context 面板等各自平台路由；详见 NEXT_TASKS 对应行。
- 共享契约、`command.rs`、compose 入口单一维护者；三线不各自扩充一套 DTO 或任务状态。

## 已核对的事实

- 2026-09-07，HEAD `92f8d92a` 加现有工作树修改：Core/Runtime、上下文、GC、搜索、恢复五条边界已按现有架构落实并定向复核。B1 继续补齐 Windows 清理期间的根身份持有、有界 helper、Unix watchdog 进程组身份持有、session Job 围栏和未知退出不记完成。真实 Rust exact-proof 宿主硬退出夹具已在 Windows/WSL Linux 通过；正式平台宿主和创建到监督就绪的窗口仍待验收。见 [核心边界与本轮验证报告](reviews/2026-09-07-worktree-review/CORE_BOUNDARIES_AND_HOST_CLEANUP.md)；[上一切片观测报告](reviews/2026-09-07-worktree-review/PROCESS_OBSERVATION_HARDENING.md) 保留当时证据。
- v0.1.0 alpha 已发布；M15 / LT-EVAL-06 证据保留，不重开、不改写。
- 任务/计划/继续/恢复的基础已在 Runtime。默认 `OperatorClosureOnly`：普通 final 结束执行段，不是模型自行持久关闭任务。
- `--max-rounds` 计量模型决策轮（`turn.model_round`）。
- 远端 CI run `34065966817` 在 `b299c6a` 上六个 job success（续审观察）；run `33986702977` 在 `12c8628` 上同样。均为远端事实，不是本地重跑，也不是全仓审查完成。

## 不在本轮

Chronicle 数据库、TaskGraph、并行 worker、向量检索、Frame-3 正式翻转、SIEVE/TinyLFU 调参、新评测总门禁、IDE/插件市场、第二套任务状态权威。
不能用"再跑一些测试"去换一个从未授予的自动完成权；GUI 不得解析工具正文推断审批，也不把 `CorePort`／恢复半事务导出给远端客户端。
