# b299c6a 续审：问题、并行路线与 .NET 原生客户端

**性质：源码定点续审与阶段提案，不是完整仓库逐行审查通过证明。**

固定源码：`b299c6a08fdb055a0148a24dea65053861df6ff1`，提交时间 `2026-09-06T23:07:02Z`。
审查交付日期：2026-09-07。讨论对象仅为本仓库，不把独立的 ContextCore 项目作为依赖或合并目标。

## 1. 实际完成与边界

实际执行了克隆，因 `Could not resolve host: github.com` 失败，记录见 `clone.log`。固定提交归档也没有取得。连接器返回了完整根目录和 19 个 crate 目录，但这不是递归全文件覆盖。此次正文读取为 19 个路径，4 个完整返回、其余为指定范围或截断响应；详细见 `READ_COVERAGE.json`。没有本地 Cargo、rustc 或 dotnet，因此未运行仓库 Rust/.NET 构建、测试和 GUI 性能比较。

远端 CI run `34065966817` 最后核对为 `completed/success`，六个 job 均成功。它是真实远端结果，不是本轮本地运行，也不能证明尚未覆盖的故障路径不存在。源码开始和结束核对相同。

执行了一个 Linux 操作系统机制探针：组长退出并被 reap 后，同组后台成员可以继续存活，而当前 watchdog 的“组长仍在”条件为 false。探针只操作自己创建的进程并确认清理，见 `probe_group_lifetime.json`。这不是仓库的 crash-resume 集成测试。

## 2. 结论

建议将下一阶段设为“基础正确性＋平台应用接口＋正式原生桌面端”并行交付。不要重写 Runtime，不把全部基础修复排为所有 GUI 工作的前置；但受影响的执行、恢复和多客户端修改能力，必须在对应问题处理后再声明受支持。

默认 GUI 建议是 **.NET 10 LTS＋Avalonia＋独立 Rust Runtime 宿主＋本地 IPC**。这是以 Windows/Linux 桌面为目标的选型建议，不是已测出的最低资源消耗冠军。Windows-only 且强调 Windows 原生集成时，改选 WinUI 3。GUI 是正式长期客户端，首版功能少不等于抛弃式验证界面。

## 3. 新发现与残余问题

### F01 / F02：监督台账没有稳定进程身份，也没有真正的清理确认

`tool-runtime/src/supervision.rs` 只记录 `pid` 和 `purpose`；`reconcile_children` 使用 `process_is_running(pid)` 后直接 `kill_process_tree(pid)`，没有读取创建时间/boot 身份。PID 数字相等不等于同一次进程创建。现有 `agent-process/src/lifecycle.rs` 已有 `ProcessIdentity`、Linux boot_id/starttime、Windows 创建时间及核对 helper，这条新车道没有复用。[S03][S12]

更重要的是，台账写失败被忽略、没有耐久同步；读失败被当成无记录；坏行被跳过；kill 后并未等待确认就把 pid 放入 killed 并删除台账。`ChildLease::Drop` 无条件删除条目。它们不能构成“清理已确认后才复用工作区”的保证。[S03][S04]

最小修复：稳定身份、受控的有界存储、显式结束确认、未决记录保留，以及启动时的类型化失败。复用既有机制，不新增第二套操作权威。无法确认身份不得对无关 PID 发信号；无法确认退出不得报清理成功。

边界：本轮没有制造真实 PID 复用事故，也不应为了测试误杀无关进程。Workspace 本身已打开多个独占日志，不能据此推导“任何第二个操作系统进程都能走到清理调用”。同进程共享 Workspace 的组合调用和恢复身份仍应分别检查。[S08]

### F03：宿主验证的 watchdog 配置没有贯通

`RecipeProofRunner::new` 构造自己的 `ProcessRunTool::new`；后者默认 `host_death_watchdog=false`。compose 安装宿主 proof runner 的路径没有将普通 dispatcher 的 watchdog 配置传进来。普通工具开启与宿主验证开启是两条不同的构造路径。[S15][S16][S04]

因此 Unix 上精确宿主验证遇到宿主硬退出，仍不能凭普通工具的配置宣称已有立即包含。台账在下次启动才清理，也不是同一保证。Windows Job 车道另行判断，不能把 Unix 结论泛化到全部平台。

修复：监督配置由可信宿主统一注入这两条现有车道，使用真实组合路径做一次验证。不要为宿主验证伪造 Core effect 身份；监督身份与权限身份分开。

### F04：watchdog 的组长条件不足，Drop 仍是无界等待

watchdog 以 `kill(leader,0)` 判活后 `kill(-leader,SIGKILL)`。这既没有创建身份保证，也会在组长已 reap 但成员仍活时跳过清理。操作系统探针已验证后一前提。[S02][S16]

`HostDeathWatchdog::Drop` 关闭写端后执行同步 `child.wait()`，没有期限。看门狗被停止、卡住或执行了不支持 marker 的宿主入口时，这会使 teardown 等待失去边界。

修复应明确监督对象、正常解除和异常整组清理，而不是简单删除判活检查后按旧 PGID 杀进程。通用命令确实允许后台后代时，要将该语义与必须受包含的宿主验证区分。Rust 宿主必须显式支持 watchdog 入口，不能让任意 .NET 可执行文件被动承担 re-exec 协议。

### F05：STORAGE-02 的 helper 内仍有发布后失败

新的 `compact_locked` 已把 seek 和状态构建移到 metadata helper 之前，这个修复方向有效；Windows 的删除后 rename 也已替换为 `MoveFileExW`。[S06]

但 Unix helper 仍为：

```text
写并同步临时 metadata
→ rename 发布
→ sync_directory(parent)?
→ 返回成功
```

若目录同步失败，外层 `?` 在交换 writer 前返回。磁盘入口可能已是新代，内存仍是旧代，且 helper 没有设置 writer.failed。显式 `compact_authority_journal` 也只是转发。[S06][S07]

正确修复是暴露“可能已发布”的阶段并围栏，或等价地保证不再把旧 writer 当健康继续使用。不能删除目录同步换取测试绿。普通 append_transition 的失败围栏仍是已有缓解，因此需要按真实调用路径验证，不夸大为每个写入都会失败。

### F06：不要把当前状态投影直接变成 GUI/SDK 真相

`StatusProjection` 在换任务时不重置 anchor_revision，后续又跨任务 `max`；A 的 revision 9 切换到 B 的 revision 1 可以继续显示 9。它是显示 bug，不会自动改写 TaskAnchor，但若 GUI 拿它作为 CAS basis 会放大错误。[S09]

headless 通过输出字符串是否包含 `denied by approval policy` 判定审批拒绝，连成功读到的普通源码/日志也会触发。它还把所有 TaskCompleted 显示为 operator_accepted；状态投影把没有持久完成的活动任务普遍说成待审阅，未充分区分运行中、让出、恢复受阻和真正产出待审。[S05][S09]

修复：平台输出中立类型化状态。实际执行阶段、任务生命周期、完成来源、审批结果和连接完整性分开。不得以 `lines()`、summary 或任意工具正文构造权威结论。出现事件缺口时标 partial/resync，不只是 stderr warning 后继续输出完整结论。

### F07：Actor 单命令串行不等于 start_long_task 整段原子

`work.rs` 依次调用 set_focus、list_tasks、replace_task_tool_requirements、user_message；最后的 user_message 只携带内容，没有期望任务 id。另一个客户端可在多次 await 之间改变活动焦点。[S10][S18]

当前单客户端正常使用可缓解，但正式 GUI/TUI/SDK 并用前应增加原子工作提交或任务/预期版本绑定，防止 A 的指令落到 B 的任务。幂等回执也不能靠远程客户端超时后重新生成 id 重发。继续使用既有 TaskManager 事务，不创建第二个任务表。

### F08：verify.run 的名称门不等于覆盖关系

错误终结现在仅由成功 verify.run 触发，已不再把普通 fs.read 成功当修复。这个进展必须承认。[S14]

但是 queue_error_verifications 仍只解析输出实体，扫描 live Error 并排入终结，没有绑定具体故障、任务、覆盖域及版本。因此另一个验证成功且提到相同文件仍可能终结无关错误。[S13][S14]

把可信验证关联投影进 Context；实体相似只能作关联或排序。证据不够则保持 live。该问题属于 Context 语义终态，不等同于 Runtime 完成 gate 已被绕过。

### F09：输入和输出的成本仍有接口外漏洞

`--prompt=-` 先 read_to_string 读完整 stdin；Runtime 后面的 256 KiB cap 不能限制此前分配。grant 文件在 stat 后整体读取，再检查大小也不能限制读取中的增长。headless 同步写 stdout/文件时，事件等待的 timeout 不控制这个 Write。[S05][S17]

平台应在接入时限制字节与解码成本，慢消费者通过有界队列、期限、断开/重新同步处理。大日志、差异、工件传引用，按需读取。不要只更换编码格式而保留无界数据流。

### F10：平台范围与文档关闭范围要重写为具体事实

当前认证 adapter 实际只处理 operation query/cancel；现有 byte seam 明确将 Named Pipe/UDS 留作后续。headless JSONL 又过滤实时 delta，不是完整的 GUI 双向协议。[S11][S19][S05]

NEXT_TASKS 声称无工程开放项，但以上残余证明其范围过宽。保留已关闭实现和真实测试，不把旧报告全部翻成未完成；只记录新增触发条件和未覆盖不变量。[S01]

## 4. 不应原样重新报告的旧问题

- release 消费 ACK 现在先执行 stamp，再 debug_assert；本轮已确认代码变化。[S14]
- process.run 的输出 EOF 已通过 outputs_closed 禁用 recv 并继续竞争退出/取消/超时。[S16]
- Windows metadata 替换不再先 unlink；这与 Unix helper 残余是两个问题。[S06]
- 成功普通读取不再直接触发错误 VerifiedFixed；残余是 verify.run 的证明范围不足。[S13][S14]
- 片段 supersession 已增加区间/覆盖判断，未知外部片段保守保留，不应再说当前全都只按 path 覆盖。[S13]

以上仅表示检查过的代码行为已改变，不宣称本轮已运行其测试，更不对未读模块给出通过结论。

## 5. 架构方向

Core 仍负责权威；Runtime 仍是每个实例唯一任务编排者；ContextEngine 保持可替换；平台负责应用入口、会话、扩展和资源访问；GUI/TUI/CLI 是客户端。无需把每层都拆成服务。

最值得新增的边界是一个与 TUI 无关的公共应用服务。可信同进程调用可以类型直连，.NET 桌面端通过独立 Rust 宿主的 IPC 调用。不要把全部 RuntimeCommand、CorePort 或恢复半事务导出到远端。

正式 GUI 的第一条链路是“连接→快照→提交→输出/计划→审批→取消/继续→审阅”，其缺失接口就进入平台任务，而不是由 GUI 私自读写 authority 文件补齐。

MCP、Skills、工具子 Agent 不必等待 Chronicle/TaskGraph。先保留现有 Capability 边界，在基本平台链路后落一个真实扩展切片；子 Agent 首版只读、独立状态、有限预算，不抢写父 TaskManager 或重复打开同一可写状态目录。

## 6. .NET GUI 推荐及边界

以本仓库目前 Windows/Linux 路线为假设，推荐 .NET 10 LTS＋Avalonia。官方支持策略列明 .NET 10 支持至 2028-11-14；框架版本与实际发布 SDK 应在实施时锁定，不从本报告的缓存页推断最新 patch。[D03]

Avalonia 是非 WebView 桌面 UI，使用自己的渲染层，默认 Skia；不是逐个包装操作系统控件。官方列有 Windows/macOS/Linux，但 OS 版本的支持等级并不相同，Linux 原生 Wayland 后端在当前文档中仍是实验 opt-in，不能将“支持 Linux”写成所有发行版/显示后端同等保证。[D01][D02]

Windows-only 且重视平台原生控件与系统体验，可选择 WinUI 3；WPF 则适合已有 WPF 代码/控件积累，但仅支持 Windows。MAUI 的官方目标列表没有 Linux，不作为本项目桌面优先方案。[D04][D05][D06]

资源收益不虚报：本轮没有任何 GUI benchmark。先交付正式客户端，并记录冷启动、整个进程树空闲内存、长会话内存斜率、流式输出时 CPU/分配与大 diff 的交互延迟。使用虚拟化、有界缓存、增量更新与受控渲染，而不是先写零分配框架。Native AOT 作为兼容性核查后的优化项，不是开发入口前置。[D07]

通信建议：C# client class library 与 Avalonia 无依赖；Windows Named Pipe、Linux UDS 使用同一有界 framing 和公共请求语义；同进程 TUI 不强制 JSON。JSON-RPC 只负责调用框架，不替代认证、任务幂等、状态完整性或崩溃恢复。[D08]

## 7. 并行实施与文档

详见 NEXT_STAGE_TASKS.md。三条工作线共用最小契约，按功能所需基础依赖合并，不用“基础全部清零”阻塞整个 GUI。

CURRENT/ROADMAP/NEXT_TASKS 应收敛成同一阶段；AGENTS 明确 Coding Agent 是第一产品客户端、GUI 非一次性验证界面。历史材料中的 projection 不应成为第二权威，这条保留；但 Chronicle→TaskGraph→worker 的旧强制顺序不再作为当前前置。

交付分别记录：实现、定向检查、默认启用、真实产品流程。CI 成功与特定故障已覆盖不是同义词。

## 附件

- FINDINGS.json：问题、触发条件、证据等级、修复边界。
- NEXT_STAGE_TASKS.md / TASKS.json：可执行阶段任务与依赖。
- READ_COVERAGE.json / READ_COVERAGE.csv：实际正文读取范围。
- SOURCES.md / SOURCE_MAP.json：固定源码与外部官方资料。
- AUDIT_STATUS.json：未完成范围与环境。
- REMOTE_CI_OBSERVED.json：远端观察结果（非原始完整 CI 日志）。
- clone.log 与 probe_group_lifetime.*：实际环境/机制记录。
