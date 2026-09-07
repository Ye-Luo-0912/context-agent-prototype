# 2026-09-07 工作区审查

审查对象为 HEAD `92f8d92af93f7478ca1a5c1de11c38519e46c6a5` **加当前未提交、未跟踪的实现**，不是仅审查该提交。开始与收尾核对时，已有 tracked diff 均为 59 个文件、2340 行新增、549 行删除。未改动现有实现、测试、任务队列或冻结证据。

本轮列出 **14 项问题：6 项 P1、8 项 P2**。其中 12 项有隔离行为探针证据，1 项为实际编译失败，1 项为 Unix 监督接线的静态确认。P1 应在相关功能合入或宣称可用前修复；P2 是可触发的正确性、协议或资源问题。它们不表示要另起“全仓审计清零”阶段。

## 范围与证据边界

范围覆盖 19 个 Rust crate、.NET 客户端、Avalonia 工作台、CI 和打包脚本；按权限、提交与恢复、输入输出、Context 生命周期和客户端链路追踪实现与调用者。不同模块的深度如下，不将入口抽查写成逐行审完所有源码。

| 模块 | 实际审查内容 |
|---|---|
| agent-contracts、agent-platform-protocol | Context/Tool/Approval/Operation 接口、错误分类、work DTO、信封与 JSON 预算；部分纯算法契约仅定向检查 |
| agent-core | 审批、租约、operation WAL 迁移、取消与 commit 屏障、输出诊断、启动对账、能力/插件准入 |
| agent-runtime | Actor 输入/工作提交/生命周期、效果结算、checkpoint/restore、状态投影、work router；能力与插件目录生命周期定向检查 |
| agent-workspace | 工作区锁、confined 句柄、预备/提交/回滚、三类效果日志、工件与输出 broker |
| agent-storage | WAL 代际、metadata 发布、writer 围栏、恢复和压缩 |
| tool-runtime | dispatcher/host policy、git、process、verify/proof、监督台账；fs/search/edit 和相关调用边界定向检查 |
| agent-process | framing、session、supervisor、进程身份、watchdog、宿主与平台 sandbox 边界 |
| agent-capability-process | process adapter、brokered IO、wire effect 意图覆盖、MCP 连接/调用/取消边界 |
| context-simple | 验证关联、scope/restore、GC/外置存储和强引用保留；索引与策略改动定向检查 |
| context-baselines | append/rolling 的入口与明确的实验语义，未重跑 A/B/C 研究 |
| context-contextcore、agent-context-service | 适配器、请求 framing、服务分发、恢复和输出校验 |
| provider-openai | Chat/Responses 流、尾帧、输出上限、重试与取消；107 项现有测试 |
| agent-compose、agent-tui | 配置接线、启动顺序、监督、headless 输入/输出/审批分类、继续与恢复 |
| agent-conformance | 现有契约检查与生产依赖/源文件窄边界检查 |
| agent-replay、agent-eval | 入口、恢复屏障、证据来源摘要、provider 配置路径；未逐行审查全部实验 runner、种子与 golden |
| Agent.Client、Agent.Desktop | DTO/codec、请求配对、断线重试、通知、审批状态和界面对象保留 |
| CI、scripts | 现有验证任务、打包来源与退出码；未发布、未生成 release/tag |

本轮在 Windows 执行。未运行 Linux watchdog/PID 复用/真实硬退出集成实验，未调用真实 provider，未重跑 M15/LT-EVAL。GUI 通过构建并执行了 ViewModel 探针，没有做真实 IPC、窗口交互、DPI 或输入法验收。正式 Rust 本地 IPC 宿主仍属 P3，不能把客户端夹具当作端到端支持证据。

## P1

### R01：只读 Git 工具可执行仓库配置的外部程序

位置：[git.rs:62](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/git.rs:62)、[git.rs:286](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/git.rs:286)。

`run_git` 直接继承 Git 配置和环境；`git.diff` 没有禁用 external diff/textconv 等外部执行路径，但工具和宿主策略将其分类为 ReadOnly。因此仓库配置 `diff.external` 后，一次获只读策略允许的 diff 就能运行任意外部程序，产生未经过进程/写审批的副作用。

**已复现：** 隔离仓库将 `diff.external` 指向探针自身的 native executable。真实 `PolicyApprovalGate::read_only()` 返回 `Allow`，`BuiltinToolDispatcher` 在没有 effect context 的情况下执行，外部程序成功写出标记文件。只修改隔离仓库，没有写主仓库配置。最初依赖 MSYS shell 的探针受执行环境限制失败；最终证据来自不需要 shell 的 native driver。

修复应建立只读 Git 命令的固定配置/环境边界，禁用 external diff、textconv、fsmonitor 等可运行程序的入口；不能只依赖固定 argv 或给结果盖 `may_mutate=false`。回归应在 ReadOnly 策略下安装会写标记的 Git driver，并断言它没有执行。

### R02：Git 先 wait 后 drain，大输出必然堵满管道并超时

位置：[git.rs:91](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/git.rs:91)、[git.rs:114](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/git.rs:114)。

stdout/stderr 被设为 pipe，但父进程先等待 `child.wait()`，待退出后才读两个流。diff 超过 pipe 容量时，Git 阻塞于写入，父进程阻塞于等待退出，最终得到 20 秒超时。之后的两个 `read_to_end` 也没有累计字节上限或覆盖整个 drain 的期限。

**已复现：** 同一隔离仓库中，直接 Git 正常返回 **816129 字节** diff；经实际 builtin 工具执行则在 **20.059892 秒**后返回 `git [...] timed out`。这会阻断普通大改动的差异审阅。

修复应在等待进程时并发排空两条 pipe，使用有界捕获/工件，并将取消、超时和退出后的尾流清理纳入同一执行期限。

### R03：SDK 丢失回执后自动重发非幂等的 continue/cancel

位置：[ResumableSession.cs:86](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/ResumableSession.cs:86)、[ResumableSession.cs:101](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/ResumableSession.cs:101)。

`RunAsync` 捕获连接/契约错误后重连，再执行同一个委托。`ContinueAsync` 和 `CancelCurrentTurnAsync` 都使用此包装，而协议请求没有原 turn 身份或幂等 key。如果原 continue 已执行、回执丢失，且任务在重连期间结束，重发可以开启第二个执行段；cancel 重发还可能命中新开始的 turn。Submit 的去重仅在宿主进程内有效，跨宿主重启也不能据此自动重发。

**已复现：** 合成传输让服务端接收并应用第一次 continue 后断开，再接受重连；实际 SDK 最终返回成功，但服务端的应用次数为 **2**。这是 SDK 重试行为证据，不是真实 provider 执行两次的声明。

修复应只自动重试可证明安全的读取；失去变更回执时返回结果未知并查询。若要重发，先补齐宿主实例/逻辑操作身份及服务端去重语义。

### R04：订阅无实际重放，却对 4096 条以内缺口返回无需重同步

位置：[work.rs:445](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/platform/work.rs:445)、[work.rs:462](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/platform/work.rs:462)。

router 仅比较 `watermark - cursor > MAX_REPLAY_WINDOW_EVENTS`，没有任何重放实现。并且 receiver 在读取 watermark 后才创建，中间发生的事件不会进入该 receiver。小缺口、未来 cursor 和 snapshot→subscribe 期间的事件都可能被当作完整流；SDK 重连还没有传 snapshot 的 watermark。

**已复现：** `cursor=1`、`watermark=2` 时返回 `resync_required=false`，新 receiver 没收到缺失事件。即使只缺一条审批/终态事件，也无法通过这条订阅补齐。

修复应先注册 receiver 再取得一致屏障；实现有限重放，或对所有无法补齐的缺口明确返回 resync。不得把 snapshot 读取失败吞成 watermark 0 的成功。

### R05：模型路径 verify.run 仍未接入 Unix host-death watchdog

位置：[verify.rs:32](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/verify.rs:32)、[registry.rs:249](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/registry.rs:249)。

dispatcher 的监督参数只传给其 `ProcessRunTool`；`VerificationRunTool::new` 又创建一个默认 `host_death_watchdog=false` 的执行器。当前 compose 已给 `RecipeProofRunner` 接线，不能覆盖这个独立的模型验证入口。Unix 宿主在模型验证期间硬退出时，这条路径没有 pipe-EOF watchdog；台账只能等下次启动对账。

**证据：静态接线确认。** 未运行 Linux 宿主硬退出复现，不能将 Windows Job 的验证外推到此路径。

修复应将宿主监督配置贯通所有复用 process runner 的验证入口，并用真实模型路径 `verify.run` 的 Linux 硬退出测试验收。归入 B1。

### R06：当前测试目标无法编译，M17 验收尚不可执行

位置：[context.rs:703](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-contracts/src/context.rs:703)、[work_control.rs:22](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/tests/actor/work_control.rs:22)、[live_walk.rs:312](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-compose/tests/live_walk.rs:312)。

`cargo check --workspace --all-targets --offline --keep-going` 实际报出 **13 个编译错误**：新增 `verify_recipe` 漏更新 contracts/core/runtime 中五处构造；work router 单元测试缺 `Route`；actor work-control 测试有私有导入、两个 disposition 类型问题和两处移动后使用；compose 的 live_walk/route_flow 仍将新返回的 `TaskId` 匹配为 `()`。

`cargo check --workspace --lib --bins --offline` 可以通过，说明“产品库/二进制可编译”和“测试目标可编译”在当前状态下是不同事实。先完成契约迁移并修复测试本身，再运行相应验收。现有 unused import/unused mut 警告也会影响 CI 的 `-D warnings` 门禁；本轮没有将 clippy 宣称为已执行。

## P2

### R07：合法 notification 被拒绝，连接却继续报告健康

位置：[AgentConnection.cs:185](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/AgentConnection.cs:185)、[AgentConnection.cs:215](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/AgentConnection.cs:215)。

`Dispatch` 在判断 kind 前要求存在 `request_id`；契约中的 notification 应省略它，客户端自己的 null-skipping serializer 也会省略。随后调用的 `Fault` 只失败 pending/关闭通知队列，不终止读循环或记录连接故障，因此重连层仍认为连接可用。

**已复现：** 合法 notification 的结果是 `delivered=False, queue_completed=True, connection_healthy=True`。修复应按 kind 校验字段，并使协议故障转为不可复用的连接状态。

### R08：配置非默认协议身份后，客户端连自己的请求都拒绝

位置：[AgentConnection.cs:96](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/AgentConnection.cs:96)、[Envelope.cs:396](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/Envelope.cs:396)。

构造器保存 options 指定的协议身份，但 `SessionEnvelope.Request` 固定使用 `DefaultProtocolIdentity`，之后又拿请求与 options 身份比较。因此合法配置任何非零默认 schema digest 都在发送前失败，无法连接使用真实约定 digest 的宿主。

**已复现：** 仅将 `SchemaDigest` 设置为 64 个 `a`，调用 Snapshot 就得到 `invalid protocol.identity: does not match the negotiated profile`。请求必须携带本连接保存的身份。

### R09：NoActiveTurn 正常取消回执总被 JSON converter 拒绝

位置：[WorkDto.cs:116](D:/Users/Ye_Luo/APP/context-agent-prototype/clients/dotnet/Agent.Client/WorkDto.cs:116)。

converter 先将 `status` 放入 HashSet，又对对象全部字段调用 `!known.Add(p.Name)`。唯一合法的 `status` 字段因此被当成重复/未知字段。

**已复现：** 解析 `{"ack":{"status":"no_active_turn"}}` 必然抛出 `JsonException: no_active_turn ack carries unknown fields`。空闲时点击取消应收到正常事实回执。修复字段白名单/重复检测，并加入这个枚举分支的跨语言 fixture。

### R10：原提交仍在执行时，重复请求拿不到 AlreadyAccepted 回执

位置：[turn.rs:123](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/actor/turn.rs:123)。

`start_work` 在查去重表之前运行 `ensure_idle()`。最需要重取受理回执的场景——原请求已受理、回执丢失、工作仍在执行——得到的是 busy 错误。work router 又把它编码为 `NotApplied`，与同一逻辑提交实际已经应用的事实相矛盾。现有重复提交测试先等 turn 完成，未覆盖这一场景。

**已复现：** HangingModel 下首次为 `Accepted`，相同 id/goal 的第二次为 `Err(InvalidRequest("agent is busy: a turn is already running"))`。应先处理已有逻辑提交的回执查询，再对新的提交检查 idle。

### R11：Context 仅按 recipe_id 终结错误，忽略版本和验证范围

位置：[reachability.rs:125](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/context-simple/src/gc/reachability.rs:125)、[engine.rs:1030](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/context-simple/src/engine.rs:1030)。

新关联字段仅保存 recipe id。一次 `check@v1` 的全量检查失败后，冷恢复使用同名 `check@v2` 的缩小范围检查，即便没有覆盖原故障，也会把旧 Error 标记为 `VerifiedFixed`。终态会让原错误退出可用上下文，并可能在之后满足 Storage GC 删除条件。

**已复现：** 失败输出携带 `recipe_id=check, recipe_revision=v1`；另一个任务的无关成功携带同 id、revision v2。引擎实际产生 **1 条 verified-fixed 转移**。此探针通过引擎的公开入口投影两份可信输出，不声称本轮运行了两份真实 recipe。

保留 recipe 关联这一改进，但还需保存并核对可信 recipe 版本/覆盖身份及足够的任务、故障关联；关系不足时不得终结。归入 B3 的剩余范围。

### R12：Context restore 接受 scope 环，完成任务时无限遍历

位置：[checkpoint.rs:65](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/context-simple/src/checkpoint.rs:65)、[scope.rs:236](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/context-simple/src/scope.rs:236)。

恢复校验只验证 parent 存在，没有验证 scope id 唯一和无环。将 Task scope 的 parent 改成自己，校验仍成功；完成任务时后代遍历不断把同一个 scope 放回 frontier，既无 visited 集，也无边数上限，会持有引擎锁循环并持续积累待关闭条目。

**已复现：** 在隔离进程恢复上述状态成功；TaskCompleted 不能返回，由探针独立线程在 250ms 后结束进程。循环结构已静态确认；没有对真实用户 checkpoint 做破坏性试验。

恢复提交前必须验证唯一 id、根节点和 DAG/tree 不变量；运行遍历也应有有界的防御性终止条件。

### R13：每次刷新审批卡都永久增加两条 GUI command

位置：[MainWindowViewModel.cs:345](D:/Users/Ye_Luo/APP/context-agent-prototype/apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs:345)、[ObservableObject.cs:76](D:/Users/Ye_Luo/APP/context-agent-prototype/apps/Agent.Desktop/Infrastructure/ObservableObject.cs:76)。

每次 ApplySnapshot 都为当前审批创建 Allow/Deny command，并加入全局 `AsyncCommandGroup._commands`；替换或清空审批列表没有移除旧 command。3 秒轮询会永久保留旧闭包和事件订阅，后续 RaiseCanExecute 还会遍历整个历史列表，违反 G3 的长期 retained-data 上限。

**已复现：** 对真实 ViewModel 应用同一张审批卡 100 次，再清空审批，command 数从 **6 增至 206**。这是对象数量证据，没有声称测量了整棵进程树内存斜率。应复用审批 command，或在卡片退出时注销并释放它们。

### R14：审批拒绝的文字推断被移到上游，仍可由外部正文伪造状态

位置：[tool.rs:1614](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-contracts/src/tool.rs:1614)、[authority.rs:277](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-core/src/authority.rs:277)、[cli.rs:328](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-tui/src/cli.rs:328)。

headless 改读 `ToolFailureClass::ApprovalDenied`，但新分类仍由通用 `failure_class_from_message` 对 `denied by approval policy` / `approval check failed` 做子串识别。sanitized 的失败工具正文会经 OutputAuthority 的诊断路径再次被盖成这个类型，CLI 于是走审批拒绝结果/退出码。类型名称没有提供来源保证。

**已复现：** 一个从未经过审批门的外部错误，正文仅引用另一个应用的 `approval check failed`；执行实际 sanitize→take diagnosis→apply diagnosis 后，`failure_class()` 为 `Some(ApprovalDenied)`。这不会授予执行权限，但会伪造审批状态和自动化结果。

应由真实审批 verdict 分支直接盖章，通用正文分类器不得生成权威审批类别。归入 P2/F06 的未闭环部分。

## 已落地修复与既有剩余范围

- **B2 已有实质修复。** metadata 发布后目录同步失败转为 RecoveryRequired，compact 路径隔离旧 writer；`compaction_publish_uncertain_failure_fences_the_writer` 本轮通过，不原样重报旧 F05。
- **B1 有进展，但仍不是完整验收。** 当前台账已保存进程创建 token，并保留未确认记录；宿主 proof runner 的配置接线已落地。除 R05 外，台账 `read_rows` 仍全量 `read_to_string`，读入时不执行 64KiB/行数上限；append/rewrite/release 没有共享串行修改保护。watchdog 的 5 秒 try_wait 之后仍有无期限 `child.wait()`（[watchdog.rs:157](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-process/src/watchdog.rs:157)）。这些仍归既有 B1，不另建阶段，也未声称做了 PID 复用或 D-state 故障注入。
- **B3 输入分配上限已有修复。** stdin/grant 使用 take(cap+1)。但未结束且未达到 cap 的 stdin 仍没有读取期限；headless 同步 writer 仍在事件等待期限之外，慢 stdout/文件可阻塞。继续归 F09/B3。
- **P1 原子提交结构已落地。** focus/requirements/首输入已进入同一 Actor 命令，旧 set-focus 多次 await 误投路径不能原样重报；R10 是新实现的重取回执缺口。
- **P2 revision 归属已有修复。** 状态投影按 task id 保存 revision，未再发现旧跨任务 max 的同一问题。订阅和审批来源保证仍见 R04/R14。
- **PACKAGE-01、MCP-01 条件项仍存在。** 打包脚本仍未把自定义 target-dir 传给 cargo，旧 dist 和 PowerShell 退出码问题未改；本轮没有发布。MCP request write 有 timeout，但取消 token 尚未覆盖整个连接/写阶段；本轮未默认启用 MCP 或做真实服务验收。
- **P3/G2 尚不能算端到端完成。** 正式宿主缺失；审批快照目前只有 request id/call name，缺少具体参数、目标和授权范围，GUI 按钮不能替代可审阅的操作详情。它们是当前路线的实现缺口，不将夹具行为描述为真实支持。

启动顺序候选已撤回：后续读取确认 `Workspace::open` 在 compose 对账前已持有 workspace-effects 的独占锁，因此“普通第二进程先杀第一个实例的子进程”不成立，不计入发现。

## 实际验证

| 命令 | 结果 |
|---|---|
| `cargo check --workspace --lib --bins --offline` | 通过；有 unused mut 警告 |
| `cargo check --workspace --all-targets --offline` | 失败，首先暴露 contracts 构造遗漏 |
| `cargo check --workspace --all-targets --offline --keep-going` | 失败，13 个编译错误，详见 R06 |
| `cargo test --offline -p agent-storage -p agent-core -p provider-openai -p agent-process -p agent-conformance --lib -j 2 -- --test-threads 2` | 因 agent-core 测试构造遗漏，编译阶段失败；没有把未执行测试计为通过 |
| 去除无法编译的 agent-core 后运行同组现有库测试 | conformance 16、process 30、storage 22、provider 107，合计 175 项通过 |
| `cargo test --offline -p agent-conformance --test dependency_boundaries -- --test-threads 2` | 3 项通过 |
| `cargo test --offline -p agent-workspace --lib -j 2 -- --test-threads 2` | 98 项通过 |
| `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj --no-restore` | 22 通过、2 失败；失败详情在 EVIDENCE.md |
| `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj --no-restore '-p:UsedAvaloniaProducts='` | 通过，0 warning/0 error；只在本次命令中跳过无法写用户目录日志的 Avalonia telemetry task |
| 现有 `scripts/doc_consistency.py`，使用 bundled Python 执行 | 通过，13 live docs/links/state |
| `git diff --check` | 失败：用户原有 `AGENTS.md:40` 文件末尾额外空行；未擅自改动 |
| 隔离 Rust/.NET 行为探针 | 见 EVIDENCE.md；包含预期失败/超时的探针，不计作现有测试通过 |

实际通过的 Rust 定向测试共 **276 项**。没有运行全仓测试、fmt/clippy 全门禁、Linux 集成或真实 provider，因此不宣称 CI、正式发布或全链路恢复已通过。

建议下一切片先处理 R01 的只读 Git 执行边界，并对 R02 的输出管道问题单独保留回归。其余问题可按已有 B1/B3/P1/P2/G3 归属进入当前队列；本报告不修改任务状态。

复现结果：[EVIDENCE.md](D:/Users/Ye_Luo/APP/context-agent-prototype/docs/reviews/2026-09-07-worktree-review/EVIDENCE.md)。源码基线摘要：[source-manifest.json](D:/Users/Ye_Luo/APP/context-agent-prototype/docs/reviews/2026-09-07-worktree-review/source-manifest.json)。
