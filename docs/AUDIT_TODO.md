# 缺陷分流

当前执行顺序由 [NEXT_TASKS.md](NEXT_TASKS.md) 前列决定：**M17 收尾 N 系列，N0 当前**。M16 与 2026-09-06/07 两轮审查的代码项保持关闭；2026-09-08 外部闭环审查（基线 `11afdd7`）的 F01–F20 见下方新表，映射 N0–N8，不重开已关闭项。

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
| **F14（→N7）** | MetricsSession Windows 无 parent 仍报 whole-tree；Linux 提前标 seen 可能漏孙进程；末样本标 idle；`_samples` 无限追加 | 覆盖范围 root_only/full_tree/unknown；一次快照建父子关系＋去重；有界采样环 | 不否定独立人工测量；不填假全树值 |
| **F15（→N6，已复核）** | 决策 supersession 按实体/子串重合排队 `Superseded`；无同任务/决策键/显式替代约束 | 实体匹配降为相关性；仅明确替代目标＋正确任务范围进终态；否则保留两条按 attention 冷却 | 不重开已修好的同任务同 probe 验证关联；Runtime 用户约束权威未被删除 |
| **F16（→N6，已复核）** | `residency.rs:315-316` Warm 路径查 keep_alive/lease；Resident TTL 路径（`gc/minor.rs`）无此检查 | 跨层共用到期保护；lease/keep_alive 范围明确；终态不可复活 | 不重调 GC 参数；不引入新淘汰算法 |
| **F17（→N6，已复核）** | `plugin.rs` `skill_read` 词法相对检查后普通 `File::open`；symlink/junction 可指包外（探针证机制）；FIFO 可在 take 前阻塞 | 复用既有 ConfinedDir/受限普通文件句柄；拒绝链接/非普通文件 | 前提是操作者安装启用的包树存在此类文件；保留双激活门/64KiB/来源版本 |
| **F18（→N6，已复核）** | `engine.rs:1705/1710` `to_summaries` 先全量投影再 `bounded_catalog`（limit=0 也投影） | limit0 早退；惰性投影或选中 ID 后复制；保持既有稳定顺序 | 惰性投影不自动把扫描 CPU 降为 O(limit)；不接数据库 |
| **F19（→N2/N3）** | `SendAsync` 只验 envelope，不调用具体 payload.Validate；反序列化缺字段用默认值 | 每个类型化 API 发送前/接受后运行 validator；两语言口径一致 | 不建反射式通用验证框架 |
| **F20（→N0/N8）** | 半帧样本 `00000009`（LE=150994944，先触发超长帧拒绝）；交错测试是两条连接；interop retry 用新 key；host 线程 join 错误被忽略 | 修准现有用例：合法长度半帧、单连接乱序、原 key 重试、执行后丢 ACK、服务线程结果必须检查 | 不建第三套 harness；测试全绿≠路径已验证 |

审查同时确认**不再原样重报**的已修复项：监督身份化台账/类型化对账/确认式清理、proof 监督接线、metadata 发布围栏、原子 StartWork、同任务同 VerificationProbe 验证关联、skill_read＋MCP/Plugin 配置缝（见 CURRENT「已修好」段与 2026-09-07 表的关闭记录）。

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
