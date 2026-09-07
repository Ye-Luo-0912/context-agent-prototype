from pathlib import Path
import json, csv, hashlib, zipfile, re

out=Path('/mnt/data/audit-platform-native-b299c6a')
sha='b299c6a08fdb055a0148a24dea65053861df6ff1'
repo='Ye-Luo-0912/context-agent-prototype'
root=f'https://github.com/{repo}/blob/{sha}/'
# Only files whose bodies were actually returned in this turn. Enumeration is separate.
rows=[
('S01','docs/NEXT_TASKS.md',[[1,220]],'partial_response_truncated',['turn271file0']),
('S02','crates/agent-process/src/watchdog.rs',[[1,330]],'full_body_returned',['turn273file0']),
('S03','crates/tool-runtime/src/supervision.rs',[[1,380]],'full_body_returned',['turn275file0']),
('S04','crates/agent-compose/src/lib.rs',[[435,515],[510,800]],'ranges_only',['turn278file0','turn277file0']),
('S05','crates/agent-tui/src/cli.rs',[[1,400]],'ranges_only',['turn279file0']),
('S06','crates/agent-storage/src/lib.rs',[[305,500],[670,965]],'ranges_only',['turn282file0','turn281file0']),
('S07','crates/agent-core/src/operation.rs',[[255,305]],'ranges_only',['turn283file0']),
('S08','crates/agent-workspace/src/lib.rs',[[1,300],[325,525]],'ranges_only',['turn284file0','turn285file0']),
('S09','crates/agent-runtime/src/status.rs',[[1,245]],'production_body_returned_tests_not_reviewed',['turn286file0']),
('S10','crates/agent-tui/src/work.rs',[], 'full_body_returned',['turn287file0']),
('S11','crates/agent-runtime/src/platform/session.rs',[[145,310]],'ranges_only',['turn288file0']),
('S12','crates/agent-process/src/lifecycle.rs',[],'full_body_returned',['turn289file0']),
('S13','crates/context-simple/src/gc/reachability.rs',[[1,250]],'ranges_only',['turn290file0']),
('S14','crates/context-simple/src/engine.rs',[[930,1070],[1535,1655]],'ranges_only',['turn291file0','turn300file0']),
('S15','crates/tool-runtime/src/proof_runner.rs',[[1,150]],'ranges_only',['turn293file0']),
('S16','crates/tool-runtime/src/tools/process.rs',[[1,180],[595,660],[665,895],[985,1170]],'ranges_only',['turn294file0','turn298file0','turn297file0','turn299file0']),
('S17','crates/agent-contracts/src/input.rs',[[1,175]],'ranges_only',['turn303file0']),
('S18','crates/agent-runtime/src/command.rs',[[1,240]],'ranges_only',['turn304file0']),
('S19','crates/agent-process/src/session.rs',[[1,85]],'ranges_only',['turn305file0']),
]
sources=[]
for sid,path,ranges,coverage,refs in rows:
    sources.append(dict(id=sid,path=path,ref=sha,requested_ranges=ranges,coverage=coverage,
                        source_references=refs,url=root+path))
web_sources=[
('D01','Avalonia: framework and custom rendering','https://docs.avaloniaui.net/docs/welcome','turn931253view0'),
('D02','Avalonia supported platforms and support tiers','https://docs.avaloniaui.net/docs/supported-platforms','turn931253view1'),
('D03','.NET support policy (.NET 10 LTS)','https://dotnet.microsoft.com/en-us/platform/support/policy','turn931253view2'),
('D04','WinUI 3','https://learn.microsoft.com/en-us/windows/apps/winui/winui3/','turn931253view3'),
('D05','WPF overview','https://learn.microsoft.com/en-us/dotnet/desktop/wpf/overview/','turn939842view0'),
('D06','.NET MAUI supported platforms','https://learn.microsoft.com/en-us/dotnet/maui/supported-platforms?view=net-maui-10.0','turn939842view1'),
('D07','Avalonia Native AOT','https://docs.avaloniaui.net/docs/deployment/native-aot','turn939842view2'),
('D08','JSON-RPC 2.0 specification','https://www.jsonrpc.org/specification','turn939842view3'),
]

findings=[
 dict(id='F01',priority='P1',title='新监督台账以 PID 冒充进程身份',evidence='STATIC_CONFIRMED',sources=['S03','S12','S04'],
 trigger='崩溃残留记录中的 PID 已被其它进程复用，再启动对账；或记录身份无法确认。',
 consequence='可能向无关进程组发信号。记录只有 pid/purpose，未使用现有创建身份令牌。',
 repair='复用 ProcessIdentity 与身份校验；无法确认身份时不得发信号，并保留可见未决状态。',
 scope_note='未执行真实 PID 复用误杀；不得为演示而针对无关进程。已有 Workspace journal 锁不等于身份校验，也不能据此宣称所有双进程启动都能进入此路径。'),
 dict(id='F02',priority='P1',title='监督记录与清理确认并不耐久、也不 fail-closed',evidence='STATIC_CONFIRMED',sources=['S03','S04'],
 trigger='写入/读取/重写台账失败、坏行、kill 未确认退出，或 lease 在子进程仍活时被 drop。',
 consequence='错误被忽略；发出 kill 后直接清除记录；Drop 无条件释放记录，启动不能据此证明清理成功。',
 repair='有界、串行、可返回错误的记录；显式确认结束后释放；未确认、损坏及读失败留账并阻止受影响工作区继续修改。',
 scope_note='记录、身份、退出确认与权限身份不同；修复应复用现有存储/监督原语，避免新建第二套 effect 权威。'),
 dict(id='F03',priority='P1',title='宿主 proof lane 未启用 Unix watchdog',evidence='STATIC_CALL_CHAIN_CONFIRMED',sources=['S15','S16','S04'],
 trigger='配置宿主 exact proof，宿主进程硬退出；RecipeProofRunner 另行构建默认 watchdog=false 的 ProcessRunTool。',
 consequence='TUI 给 builtin dispatcher 的开启配置未贯通到宿主验证执行器；下一次启动清理不等于死亡时立即包含。',
 repair='由可信组合根向普通执行与宿主验证传入同一监督配置/句柄；以真实宿主路径验证，而不只测工具或看门狗 helper。',
 scope_note='此结论针对 Unix 宿主证明车道。Windows Job 路径独立；普通任务未必配置 exact proof。'),
 dict(id='F04',priority='P1',title='watchdog 的组长存活判定不足，Drop 等待无界',evidence='STATIC_PLUS_OS_MECHANISM_PROBE',sources=['S02','S16'],
 trigger='组长已 reap、组内成员仍活；PID 被复用；或看门狗自身卡住/未执行预期入口。',
 consequence='kill(leader,0) 不能证明创建身份，也不能证明整个组已退出；Drop 中同步 wait 无 deadline。',
 repair='监督对象与身份、正常解除、整组清理分别定义；有界确认及未决责任保留；新宿主显式支持 watcher 入口。',
 scope_note='probe_group_lifetime.json 仅证明 Linux 机制，不是仓库集成测试。通用命令若允许后台后代，要与必须被包含的宿主验证语义分开。'),
 dict(id='F05',priority='P1',title='STORAGE-02 残余：rename 成功、目录 sync 失败仍留旧 writer',evidence='STATIC_CONDITIONAL_PATH_CONFIRMED',sources=['S06','S07'],
 trigger='Unix persist_authority_metadata 内 replace_file 成功，随后 sync_directory 返回错误。',
 consequence='compact_locked 提前返回，没有交换 writer；显式 compact 路径也未设置同等恢复围栏。',
 repair='携带发布阶段的错误或在可能已发布时隔离 writer；保留目录同步，不得靠删同步调用消除报错。',
 scope_note='移动 seek 的修复有效，但没有覆盖 helper 内部发布后失败。普通 append_transition 的上层失败围栏仍是既有保护。未本地故障注入。'),
 dict(id='F06',priority='P2',title='状态投影与 headless 结果仍混用展示文字和权威状态',evidence='STATIC_CONFIRMED',sources=['S09','S05'],
 trigger='从高 revision 任务切换到低 revision 任务；成功工具正文包含 denied by approval policy；事件缺口或不同完成来源。',
 consequence='anchor_revision 跨任务取 max；普通内容可误报审批拒绝；所有 TaskCompleted 被标 operator_accepted；缺口只写 stderr 继续。',
 repair='按任务绑定 revision；使用类型化审批/终态事实；连接、turn、task、closure cause 分开；缺口显式标 partial 并同步。',
 scope_note='这是 read model/退出结果错误，不等同于 Core 审批或持久完成被绕过。'),
 dict(id='F07',priority='P1_BEFORE_MULTI_CLIENT_WRITES',title='公共 work 流程由多次命令组成，不能作为原子多客户端入口',evidence='STATIC_INTERLEAVING_RISK',sources=['S10','S18'],
 trigger='A 的 set_focus/list/requirements/user_message 之间，另一个控制客户端改变活动任务。',
 consequence='无目标任务身份的 user_message 可能落到另一任务；Actor 单条命令串行不保证整段客户端流程原子。',
 repair='加入绑定任务/预期版本的提交或原子 start_work 入口；受理回执、有限幂等和未知结果处理由公共层统一。',
 scope_note='当前单 UI 串行使用可缓解；未声称已在当前 TUI 复现。GUI/TUI 同时控制前必须解决。'),
 dict(id='F08',priority='P2',title='上下文错误终结只加了 verify.run 名称门，仍以实体匹配覆盖',evidence='STATIC_SEMANTIC_RISK_CONFIRMED',sources=['S13','S14'],
 trigger='某个成功 verify.run 输出与另一条未解决故障共享实体，但没有证明同一故障/域/版本。',
 consequence='queue_error_verifications 可把无关错误排入 VerifiedFixed，影响以后普通召回。',
 repair='由可信验证事实关联故障/任务/覆盖域/资源版本；不足以建立关联时仅保留弱相关，不授予终态。',
 scope_note='成功 fs.read 已不再触发，此旧问题不能原样重报；Runtime 的完成 gate 与 Context 语义终态不是一层。'),
 dict(id='F09',priority='P2',title='输入/输出桥存在边界外读入和阻塞',evidence='STATIC_CONFIRMED',sources=['S05','S17'],
 trigger='--prompt=- 输入无限/极大；grant 文件在 stat 后增长；stdout 或 JSONL sink 卡住。',
 consequence='Runtime 的输入 cap 不限制此前 read_to_string 分配；同步 Write 不受事件接收 select 的超时保护。',
 repair='接入时按字节限额读取；输出从执行循环隔离为有界队列/写超时，慢消费者断开或报告缺口；不复制给 GUI。',
 scope_note='未执行内存耗尽或堵塞生产输出的压力测试。不要把合法小消息 JSON 编码优化当成主要问题。'),
 dict(id='F10',priority='P2_DELIVERY_GAP',title='完整平台客户端接口尚未落地；文档的关闭范围过宽',evidence='SOURCE_SCOPE_AND_DOCUMENT_MISMATCH',sources=['S01','S11','S19','S10'],
 trigger='把 operation query/cancel adapter 或 headless JSONL 当成完整 GUI API。',
 consequence='缺任务提交、审批、快照衔接、正文读取等闭环；无工程开放项不足以概括上述残余。',
 repair='以正式 GUI 为消费者补最小公共应用服务和本地通道；活动文档标注具体已验范围与残余，不再全面声称清零。',
 scope_note='不是要求推翻现有平台协议；Named Pipe/UDS 在当前源码中明确仍是后续后端。'),
]

report=r'''# b299c6a 续审：问题、并行路线与 .NET 原生客户端

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
'''
(out/'REPORT.md').write_text(report,encoding='utf-8')

# Each task is proposed, never represented as current implementation.
tasks=[
 dict(id='C0',lane='shared_contract',title='收敛平台＋基础＋正式 GUI 的阶段与最小契约',depends_on=[],
 files=['AGENTS.md','docs/CURRENT.md','docs/ROADMAP.md','docs/NEXT_TASKS.md','crates/agent-platform-protocol/src/'],
 outcome='不同入口使用同一操作、身份和快照语义，GUI 可直接进入长期实现。',
 steps=['把“无工程开放项”改为具体已验范围；保留旧测试事实及新残余，不重开整个 M16。','先约定 submit/continue/cancel/snapshot/subscribe/approval response 的有限 DTO、身份、错误与大小上限。','把请求受理、应用、任务完成和清理确认分开；列清 supported/unsupported，不先定义所有未来 namespace。','同一份契约示例供 Rust/C# 使用；共享字段修改由一个负责人合入。'],
 acceptance=['已有文档检查通过。','Rust/C# 对同一组小型消息示例的含义一致；未实现操作明确拒绝。'],
 checks=['python scripts/doc_consistency.py'],stop='不新建文档治理框架；不把全部平台协议设计完作为后续开始条件。'),
 dict(id='B1',lane='foundations',title='监督身份、台账、宿主验证接线与有界清理',depends_on=[],
 files=['crates/tool-runtime/src/supervision.rs','crates/tool-runtime/src/proof_runner.rs','crates/tool-runtime/src/tools/process.rs','crates/agent-process/src/watchdog.rs','crates/agent-process/src/lifecycle.rs','crates/agent-compose/src/lib.rs'],
 outcome='不会凭旧 PID 误杀其它进程，未确认清理不会丢监督记录，真实宿主 proof 路径应用同一监督策略。',
 steps=['读取 F01–F04 对应调用链及现有 process-journal；复用稳定身份 helper，旧无身份记录默认不得 kill。','台账记录/读取/结束返回 Result；限制大小与行数，串行化修改，保证必要耐久；错误不能静默当空。','以明确 finished/reaped receipt 释放记录，Drop 仅承担保守清理而不是伪造确认。','将监督配置贯通普通 dispatcher 与 RecipeProofRunner；不把标记 re-exec 隐式依赖传播到任意客户端 exe。','修正组长、成员、正常解除语义；有界等待 watcher，无法确认则保留责任。'],
 acceptance=['用自己创建的进程和错误 creation token 证明不发送错误 kill，不实际攻击别的进程。','损坏、不可写、读失败和 kill 无确认均不能输出清理成功。','从真实 Rust 宿主硬退出的 exact-proof 路径验证清理；普通通用后台进程按声明策略单独处理。','组长退出、成员仍活的用例符合明确监督契约。'],
 checks=['cargo test -p agent-process','cargo test -p tool-runtime supervision','cargo test -p agent-compose'],stop='不新建通用 Scheduler；同一修复可以拆小 PR，但受支持的执行/恢复声明不得提前。'),
 dict(id='B2',lane='foundations',title='处理 metadata 已发布但同步失败的围栏',depends_on=[],
 files=['crates/agent-storage/src/lib.rs','crates/agent-core/src/operation.rs'],
 outcome='一次压缩返回不确定错误后，不会继续健康地向旧代写入。',
 steps=['保留 seek 前移和 Windows 替换修复；沿 helper 内部 rename→sync_directory 看错误阶段。','让发布不确定显式传播或隔离 writer；显式 compact 与追加触发压缩都不能留下健康旧 writer。','复用既有 fault 注入点，只新增该发布切点所需的最小注入。'],
 acceptance=['rename 成功且目录同步失败时，后续写明确拒绝或已安全切到新代；不得留旧代健康状态。','重新打开只承认已验证代际；不删除同步屏障、不选最高 gN 猜恢复。'],
 checks=['cargo test -p agent-storage','cargo test -p agent-core'],stop='只修发布语义，不新增数据库、Chronicle 或另一套日志协议。'),
 dict(id='B3',lane='foundations',title='Context 验证关联与运行边界',depends_on=[],
 files=['crates/context-simple/src/engine.rs','crates/context-simple/src/gc/reachability.rs','crates/agent-contracts/src/execution_facts.rs','crates/agent-tui/src/cli.rs'],
 outcome='错误不会被同实体的无关验证终结；外部输入/输出不能在进入 Runtime 前后绕过限额。',
 steps=['保留已修好的 ACK、EOF 和片段覆盖逻辑，不原样重做。','对成功 verify.run 投影可信任务/故障/覆盖关系；关系不充分则只关联，不设置 VerifiedFixed。','stdin 与 grant 读取在读入时计费；输出 sink 不阻塞执行/取消通道，定义慢消费者和断线结果。','不要把终端输入/输出 helper 直接当成正式 GUI 的客户端实现。'],
 acceptance=['相同文件上不同故障/域的成功验证不终结无关故障；真正匹配的证明可终结。','超大/不结束输入、慢输出均有边界；没有进行全量分配之后才报超限。'],
 checks=['cargo test -p context-simple','cargo test -p agent-tui'],stop='不重调 GC 阈值，不同时引入 BM25、向量或缓存算法。'),
 dict(id='P1',lane='platform',title='与 TUI 无关的原子工作提交与受理回执',depends_on=['C0'],
 files=['crates/agent-tui/src/work.rs','crates/agent-runtime/src/command.rs','crates/agent-runtime/src/actor/commands.rs','crates/agent-compose/src/lib.rs'],
 outcome='TUI、GUI、SDK 不能把指令误投给另一客户端刚切换的任务。',
 steps=['将共享工作入口移入公共应用层；没有复用需求前不拆很多 crate。','实现原子 start_work 或显式 task/expected revision 提交；复用既有 TaskManager prepare/commit。','返回稳定受理身份；相同 client request id＋内容重试有界去重，异内容拒绝。','明确持久范围及过期/重启 unknown：不能承诺未实现的跨重启 exactly-once，也不能自动换 ID 重放。','没有可重放回执时要求先查询或人工处理未知结果；不把接收记录当成权限提升。'],
 acceptance=['两个客户端交错 SetFocus/Submit 时不能跨任务投递。','重复请求不能偷偷再执行一次，冲突身份被拒绝。','原有单客户端继续任务身份不变；无需第二 TaskManager。'],
 checks=['cargo test -p agent-runtime','cargo test -p agent-tui'],stop='不一次远程导出整个 RuntimeCommand，不公开恢复半事务或 CorePort。'),
 dict(id='P2',lane='platform',title='类型化快照、增量事件、审批与结果',depends_on=['C0','P1'],
 files=['crates/agent-runtime/src/status.rs','crates/agent-contracts/src/event.rs','crates/agent-core/src/approval.rs','crates/agent-tui/src/cli.rs','crates/agent-runtime/src/platform/'],
 outcome='新客户端接入、重连、慢消费后可以显示正确状态，而不解析终端文字。',
 steps=['修任务 revision 的归属；区分 run/turn/task/closure source/connection completeness。','移除从任意 ToolOutput 文本推断审批拒绝；使用可信审批结果和现有类型化事实。','提供一致快照＋watermark，以及该点后的事件；定义有限重放窗口及 resync_required。','实时文本流偏移与耐久事件序列分开；不能把 live-only delta 当已提交事实。','审批响应绑定 request/run/operation 和认证会话，重连可查询 pending；迟到/重复应返回当前事实。','慢消费者限制队列，允许合并展示进度，不允许静默抹掉审批/终态；不得阻塞 Actor。'],
 acceptance=['A revision9→B revision1 展示与 API 返回 B=1。','成功工具正文含审批拒绝短语不改变真实审批状态。','订阅与快照并发不漏关键事件；缺口被明确报告，安全错误不被较早普通拒绝遮盖。'],
 checks=['cargo test -p agent-runtime','cargo test -p agent-tui'],stop='只构建有界可重建投影；不建 Chronicle 数据库，不让投影反向提交 effect。'),
 dict(id='P3',lane='platform',title='正式 Rust 宿主与本地双向 RPC',depends_on=['C0','P1','P2'],
 files=['crates/agent-runtime/src/platform/session.rs','crates/agent-process/src/session.rs','crates/agent-platform-protocol/src/','crates/agent-compose/src/lib.rs','proposed: crates/agent-host/'],
 outcome='原生 GUI 与其它入口连接同一个工作区宿主，不各自打开一份可写运行状态。',
 steps=['建立很薄的宿主可执行文件或等价现有入口；生命周期、workdir 单实例、watchdog marker 在 Rust 宿主负责。','Windows Named Pipe、Linux UDS 使用相同 byte framing；将 OS 特有后端隔离。先交付开发平台后补另一平台，不同时做 HTTP/TCP/gRPC。','连接 ACL/peer 身份和会话授权在服务端落实；同用户不等于任意操作已获 grant，客户端不得自报 UserSteering 提权。','定义帧与 decoded DOM 上限、最大并发/队列、读写期限；请求到达后可受理返回，接收循环不能等整个任务结束才读 cancel。','RPC 路由进入公共应用层；已有 operation query/cancel 保持语义，MCP 外部 wire 不必改成私有协议。','明确关闭窗口、断线、宿主退出、task cancel 的区别；提供显式继续后台运行或停止策略。'],
 acceptance=['半帧/粘帧/超大帧/无权会话正确处理；控制消息不会被日志流无限阻塞。','同工作区第二个客户端附着既有宿主，不产生第二个 authority writer。','基础 B1/B2 未完成时可开发只读连接，但不能开放相关可靠执行/恢复承诺。'],
 checks=['cargo test -p agent-platform-protocol','cargo test -p agent-process','cargo test -p agent-runtime'],stop='不做系统级常驻服务、不默认公网监听，不复制 codec 和 authority。'),
 dict(id='G1',lane='native_gui',title='.NET 客户端库与正式 Avalonia 外壳',depends_on=['C0'],
 files=['proposed: clients/dotnet/Agent.Client/','proposed: apps/Agent.Desktop/','proposed: global.json'],
 outcome='第一版就是正式原生客户端；它的通信层可被其它 .NET 应用复用。',
 steps=['以 .NET10 LTS 建立 class library 与 Avalonia app；锁定实施时确认的稳定框架/SDK版本。','Agent.Client 不依赖 Avalonia，不 P/Invoke 整个 Runtime；DTO 使用共同规范和跨语言样例，避免重复事实模型。','实现请求关联、取消等待与显式取消命令的区别、事件流和帧大小限制；实现与目标 OS 匹配的管道/socket客户端。','先用同一 DTO 的有限 fixture 驱动布局，P1/P2可用后立即连接真实宿主；fixture不是另一套模拟执行器。','正式窗口包含任务选择、输入/输出、计划、状态；异步读写不占 UI 线程。'],
 acceptance=['client library 可在没有 GUI 的测试程序使用。','同一正式 GUI 连接真实 Runtime 获取快照并提交/继续任务；不替换为第二个验证 GUI。','没有 WebView/浏览器后端作为主界面依赖。'],
 checks=['dotnet build clients/dotnet/Agent.Client/Agent.Client.csproj','dotnet build apps/Agent.Desktop/Agent.Desktop.csproj'],stop='路径为拟新增而非已有；不先做 IDE、插件 UI SDK 或全量自绘控件库。'),
 dict(id='G2',lane='native_gui',title='审批、取消、审阅与恢复完整操作链',depends_on=['G1','P2','P3','B1','B2'],
 files=['proposed: apps/Agent.Desktop/','proposed: clients/dotnet/Agent.Client/','crates/agent-runtime/src/instance.rs'],
 outcome='用户能执行真实开发任务，并知道改动、检查、未验证状态以及可恢复点。',
 steps=['审批卡显示平台返回的绑定操作/范围；点击后等待真实回执，不本地宣布授权。','展示工具/模型运行、预算让出、待审阅、证据完成、操作员接受和恢复受阻等不同状态。','差异与工件通过授权平台按需读取；保留用户已有修改，未知归属明确标记。','重开窗口走快照/事件同步；冷恢复通过完整 RuntimeInstance事务，不从文件挑字段恢复。','实现中文输入法、复制、键盘操作、DPI及大文本正常行为，不仅测试截图。'],
 acceptance=['GUI→任务→真实工具/审批→取消/续跑→差异审阅端到端成立。','丢连接不会让 pending 审批自动通过；客户端崩溃不伪造任务终态。','一次含用户预存修改的冷恢复与继续不重复副作用。'],
 checks=['dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj','cargo test -p agent-compose'],stop='不得增加第二执行器；不做全量通用代码编辑器，不因 GUI 增加绕过 Core 的文件写入口。'),
 dict(id='G3',lane='native_gui',title='低资源使用的正式工作台与 Context 检查',depends_on=['G1','P2','B3'],
 files=['proposed: apps/Agent.Desktop/','crates/agent-runtime/src/platform/'],
 outcome='长会话、大 diff 和上下文查看不会导致全历史反复传输、解析和渲染。',
 steps=['可视列表虚拟化同时限制底层保留数据；流式文字按小窗口合并，不每 token 重建完整Markdown/视觉树。','大正文只传 locator、元数据与分页片段；视图关闭释放缓存，设置有界引用缓存。','Context 面板先只读显示来源、表示类型、实际曝光、片段范围和恢复状态；不显示不存在的模型内部注意力。','记录全进程树空闲内存、长会话斜率、输出时CPU/分配、大diff响应；不在没有基线时宣称原生必然最省。','AOT/裁剪单独检查控件依赖和序列化兼容；不把AOT设为首个窗口可用的前置。'],
 acceptance=['固定大型会话/差异用例的 retained data 有界，滚动不随所有历史全量重绘。','工具/审批/终态事件不为流畅显示而静默丢失。','记录环境与实际测量；没有测量的指标保持NOT_RUN。'],
 checks=['dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release'],stop='不建遥测平台，不写零分配通用UI，不把研究性GC算法的胜出当GUI完成标准。'),
 dict(id='E1',lane='platform_extension',title='一个真实外部能力与按需 Skill 的最小闭环',depends_on=['P3','G1'],
 files=['crates/agent-capability-process/src/mcp.rs','crates/agent-runtime/src/capability/mod.rs','crates/agent-runtime/src/plugin.rs','crates/agent-contracts/src/plugin.rs'],
 outcome='平台不只是任务入口：正式客户端能使用一个实际外围能力，Skill不常驻所有上下文。',
 steps=['先现场重读这些未在本轮完整审查的模块与测试，再复用已有 Capability/Plugin 基础。','选一个只读外部能力或MCP server，配置→授权→发现→按需加载→调用→有界结果→关闭。','任何被声明支持的 MCP 路径都先处理写/连接/读取消，不以“默认未开启”豁免显式使用。','Skill 提供有来源、版本与范围约束的按需正文读取；不提升为system权限，脚本仍由已有工具审批执行。'],
 acceptance=['新增一个能力无需向RuntimeActor加入该业务特判。','未加载能力不把全部schema/Skill正文注入每轮请求。','故障和停机不隐去未确认清理。'],
 checks=['cargo test -p agent-capability-process','cargo test -p agent-runtime capability'],stop='不建插件市场；不要求子Agent/DAG/递归改进才能完成本切片。'),
 dict(id='R1',lane='joint_delivery',title='Rust＋.NET 来源绑定发布及正式使用收口',depends_on=['B1','B2','B3','P3','G2','G3'],
 files=['scripts/dist.sh','scripts/dist.ps1','.github/workflows/package.yml','proposed: native desktop packaging','docs/CURRENT.md','docs/COMPATIBILITY.md'],
 outcome='安装包里的客户端/宿主确属本次构建，版本匹配，真实使用与验证范围明确。',
 steps=['修自定义target未传Cargo、陈旧dist混入、PowerShell退出码处理；不在旧目录拼装包。','记录Rust源码SHA、.NET/协议版本、构建配置、平台依赖与checksum；不把checksum当构建来源证明。','现有跨平台CI加最小C#构建/协议相容项，不重跑M15或建立新统计总门禁。','正式GUI完成小bug、多文件功能、断线/冷恢复续跑；含用户原有修改。真实provider不可用写NOT_RUN。','把代码实现、定向测试、默认启用、正式使用四类状态分别更新。'],
 acceptance=['一个包可在声明支持环境连接对应宿主，读取不支持版本时明确拒绝。','源码/构建来源可追溯；失败构建不输出成功包。','真实检查记录完整，未跑项不被摘要成全绿。'],
 checks=['cargo fmt --check','cargo clippy --workspace --all-targets -- -D warnings','cargo test --workspace','dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release'],stop='这些为集成/发布检查，不是每次改文档都执行；不要求所有未来扩展实现才发布当前正式GUI。'),
]
for t in tasks:
    t['status']='PROPOSED_NOT_IMPLEMENTED_BY_THIS_AUDIT'

text='''# 下一大阶段：基础、平台、正式原生 GUI 并行任务脚本\n\n> 阶段名建议：M17（提案编号，不表示仓库已采纳）。基线 b299c6a08fdb055a0148a24dea65053861df6ff1。\n> 本文件是可交给 Coding Agent 执行的任务说明，不是会自动改仓库的 shell 脚本。全部新任务状态为提案；执行前对新 HEAD 逐项确认已落地内容并跳过，不回退用户代码。\n\n## 开工规则\n\n先运行 `git status --short` 和 `git rev-parse HEAD`，保留用户改动。读取当前工单涉及的模块、调用者和测试；本轮没读到的模块必须现场补读，不宣称完整仓库已审查完。\n\n三条线共享一组小契约，不各自设计任务状态。C0先约定首组消息后，B1/B2/B3与P1、G1可并行；正式GUI代码从第一批起保留。共享的contracts/command/compose入口指定单一维护者，其它线提交小接口需求，不在同一巨型文件上相互覆盖。\n\nP3与GUI可先跑只读/有限路径；G2涉及执行与恢复保证必须等待B1/B2对应验收。基础其它研究性优化不阻塞平台和GUI。E1是平台扩展交付，不强制阻塞第一个GUI发布。\n\n每个工单达到验收即停止扩展。下面测试命令是执行建议，本审查未执行；新项目路径和测试项目均明确为拟新增。已有测试过滤命令必须检查实际选中测试数，零测试通过不能写作验收通过。\n\n## 概览\n\n| 工单 | 线 | 交付 | 依赖 |\n|---|---|---|---|\n'''
for t in tasks:
    text+=f"| {t['id']} | {t['lane']} | {t['title']} | {', '.join(t['depends_on']) or '可开始'} |\n"
for t in tasks:
    text+=f"\n## {t['id']} — {t['title']}\n\n**用户结果：** {t['outcome']}\n\n**代码入口／拟新增路径：** " + '；'.join('`'+p+'`' for p in t['files'])+'。\n\n**执行步骤：**\n\n'
    text+='\n'.join(f'{i+1}. {v}' for i,v in enumerate(t['steps']))+'\n\n**验收：**\n\n'
    text+='\n'.join('- '+v for v in t['acceptance'])+'\n\n**建议检查（尚未执行）：**\n\n```text\n'+'\n'.join(t['checks'])+'\n```\n\n**做到这里停止：** '+t['stop']+'\n'
text+='''\n## 阶段后候选，不作为本阶段前置\n\n只读工具子 Agent 可以沿平台委派接口推进：独立状态、有限预算、无递归、父操作取消传播，结果返回证据和工件。不得成为父TaskManager第二写入者，也不能另开同一可写工作区日志。先通过受限资源视图或独立快照工作区获得输入，不先做多写者并行。\n\nContext/GC/搜索算法研究：候选质量不足再试字段化词法评分；重复正文挤预算再试依赖组边际收益；恢复成本高再试缓存准入/淘汰；维护阻塞才增脏对象与到期队列。每次只替换一个主要机制，使用既有观测记录完整任务成本，不作为GUI交付前置。\n\n## 每项交付回执\n\n报告：源码SHA；变更文件；用户新增动作；执行过的命令及测试数量；实际结果；默认启用状态；真实流程是否运行；未验证平台/故障；下一工单。不要把“函数存在”“单测通过”“默认接线”“真实使用通过”合并成一个Done。\n'''
(out/'NEXT_STAGE_TASKS.md').write_text(text,encoding='utf-8')
(out/'TASKS.json').write_text(json.dumps(tasks,ensure_ascii=False,indent=2),encoding='utf-8')
(out/'FINDINGS.json').write_text(json.dumps(findings,ensure_ascii=False,indent=2),encoding='utf-8')
(out/'READ_COVERAGE.json').write_text(json.dumps(sources,ensure_ascii=False,indent=2),encoding='utf-8')
with (out/'READ_COVERAGE.csv').open('w',encoding='utf-8-sig',newline='') as f:
    w=csv.writer(f);w.writerow(['id','path','ref','requested_ranges','coverage','source_references'])
    for s in sources:w.writerow([s['id'],s['path'],s['ref'],json.dumps(s['requested_ranges']),s['coverage'],';'.join(s['source_references'])])
source_map={'repository_files':sources,'external_primary_sources':[dict(id=i,title=t,url=u,source_reference=r) for i,t,u,r in web_sources],
'historical_attachment':{'title':'审查仓库完整性.txt','role':'historical_proposal_only','source_reference':'turn266file0','warning':'其中旧基线、跑测和Chronicle/TaskGraph顺序不作为当前事实/硬前置'}}
(out/'SOURCE_MAP.json').write_text(json.dumps(source_map,ensure_ascii=False,indent=2),encoding='utf-8')
st='# 来源与阅读范围\n\n固定源码链接如下；范围限定见 READ_COVERAGE。没有取到的代码不在已审阅计数中。\n\n'
for s in sources:st+=f"- [{s['id']}] `{s['path']}`；范围 {s['requested_ranges'] or '全文返回'}；{s['coverage']}。\n  {s['url']}\n"
st+='\n## 外部一手资料（2026-09-07 查询）\n\n'
for i,t,u,r in web_sources:st+=f'- [{i}] {t}\n  {u}\n'
st+='\n## 历史材料\n\n用户提供的《审查仓库完整性》只用于理解先前提案。它支持“projection 不应成为第二权威”的设计意图；其旧执行顺序及声称的跑测结果没有被自动继承。本轮正文结论以固定源码、远端CI观察和明确标注的机制探针为依据。\n'
(out/'SOURCES.md').write_text(st,encoding='utf-8')
ci={'kind':'CONNECTOR_OBSERVATION_NOT_LOCAL_EXECUTION','head_sha':sha,'run_id':34065966817,'status':'completed','conclusion':'success','run_updated_at':'2026-09-06T23:27:29Z','source_reference':'turn308file0','jobs':[{'id':id,'name':name,'status':'completed','conclusion':'success'} for id,name in [(101574649777,'fmt / clippy / build (windows-latest)'),(101574649884,'document consistency'),(101574649913,'fmt / clippy / build (ubuntu-latest)'),(101575111277,'test (windows-latest, part full)'),(101575111287,'test (ubuntu-latest, part 1)'),(101575111313,'test (ubuntu-latest, part 2)')]]}
(out/'REMOTE_CI_OBSERVED.json').write_text(json.dumps(ci,ensure_ascii=False,indent=2),encoding='utf-8')
status={'repository':repo,'review_date':'2026-09-07','pinned_commit':sha,'tree':'1c4a4d28378b882d52dfbdf9e9fdeb63271419aa','head_unchanged_at_final_check':True,
 'full_repository_audit_completed':False,'complete_checkout':False,'clone_attempted':True,'clone_failure':'Could not resolve host: github.com','archive_obtained':False,
 'inventory_scope':'complete root and immediate crates listing only; NOT recursive file coverage','workspace_crates_enumerated':19,
 'body_paths_read':len(sources),'full_body_returned_paths':sum(s['coverage']=='full_body_returned' for s in sources),
 'coverage_note':'Requested ranges record connector calls, not an assertion that every request returned all lines. NEXT_TASKS was response-truncated. Only 4 small files returned complete bodies; code reasoning focused on the listed paths.',
 'local_rust_build_test':'NOT_RUN: no checkout and no cargo/rustc','local_dotnet_build_test':'NOT_RUN: no dotnet and no GUI implementation','remote_ci':ci,
 'executed_probe':'probe_group_lifetime.json (Linux mechanism only; not repository code execution)',
 'repository_changed':False,'github_writes':False,'artifact_contents':'review and proposed task documents; NOT repository source archive or implementation patch'}
(out/'AUDIT_STATUS.json').write_text(json.dumps(status,ensure_ascii=False,indent=2),encoding='utf-8')
(out/'README.md').write_text('''# 审查与下一阶段任务包\n\n从 [REPORT.md](REPORT.md) 看代码问题，从 [NEXT_STAGE_TASKS.md](NEXT_STAGE_TASKS.md) 开始实施。\n\n这是固定提交 b299c6a 的定点续审和阶段提案，不是完整源码ZIP，不是全仓审查通过证明，也没有实现或推送新功能。\n\n选型建议：保留Rust执行系统，采用.NET10＋Avalonia正式原生客户端，以受限本地IPC连接；平台、基础、GUI按功能依赖并行。\n\n实际覆盖和未跑项见 [AUDIT_STATUS.json](AUDIT_STATUS.json)、[READ_COVERAGE.csv](READ_COVERAGE.csv)；问题见 [FINDINGS.json](FINDINGS.json)，源码和外部资料见 [SOURCES.md](SOURCES.md)。\n\n`probe_group_lifetime.py` 是已经执行过的隔离Linux机制探针；其结果不等于仓库集成测试。`build_report.py` 只是生成本包的脚本，不是仓库修改脚本。\n''',encoding='utf-8')

# Validate the task graph and references, without misrepresenting this as testing repository code.
ids={t['id'] for t in tasks}
assert len(ids)==len(tasks)
byid={t['id']:t for t in tasks}
visited=set();stack=set()
def visit(x):
    assert x in ids
    if x in stack:raise ValueError('dependency cycle')
    if x in visited:return
    stack.add(x)
    for d in byid[x]['depends_on']:visit(d)
    stack.remove(x);visited.add(x)
for i in ids:visit(i)
sourceids={s['id'] for s in sources}|{s[0] for s in web_sources}
for f in findings:assert set(f['sources'])<=sourceids
for fn in ['REPORT.md','NEXT_STAGE_TASKS.md']:
    txt=(out/fn).read_text(encoding='utf-8')
    assert txt.count('```')%2==0,fn
    for ref in re.findall(r'\[(S\d+|D\d+)\]',txt):assert ref in sourceids,ref
for p in out.glob('*.json'):json.loads(p.read_text(encoding='utf-8'))
validation={'artifact_validation_only':True,'task_count':len(tasks),'findings_count':len(findings),'source_path_count':len(sources),'task_dependency_graph':'acyclic','source_ids':'resolved','json_files':'parsed','repository_tests':'NOT_RUN'}
(out/'ARTIFACT_VALIDATION.json').write_text(json.dumps(validation,ensure_ascii=False,indent=2),encoding='utf-8')
# Hash all contents except the manifest itself.
manifest={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(out.iterdir()) if p.is_file() and p.name!='SHA256SUMS'}
(out/'SHA256SUMS').write_text(''.join(f'{v}  {k}\n' for k,v in manifest.items()),encoding='utf-8')
z=out.parent/'audit-platform-native-b299c6a.zip'
with zipfile.ZipFile(z,'w',zipfile.ZIP_DEFLATED) as f:
    for p in sorted(out.iterdir()):
        if p.is_file():f.write(p,out.name+'/'+p.name)
print(json.dumps({'zip':str(z),'size':z.stat().st_size,'files':len(list(out.iterdir())),'validation':validation},ensure_ascii=False,indent=2))
