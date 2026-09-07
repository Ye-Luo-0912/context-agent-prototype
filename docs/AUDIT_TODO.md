# 缺陷分流

当前执行顺序由 [NEXT_TASKS.md](NEXT_TASKS.md) 前列决定：**M17 三线首批（C0/B1/B2）**。2026-09-06 深入续审代码项与 M16-00–07 保持关闭；2026-09-07 外部续审（基线 `b299c6a`，部分源码静态审查）的残余见下方新表，映射为 B1/B2/B3/P1/P2，不重开已关闭项。

原报告与探针：[reviews/2026-09-06-deep-audit/REVIEW.md](reviews/2026-09-06-deep-audit/REVIEW.md)。
2026-09-07 续审原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)（问题/触发条件/证据等级见同目录 FINDINGS.json）。
建议回归按 [TEST_MATRIX.md](reviews/2026-09-06-deep-audit/TEST_MATRIX.md) 补进现有 crate，不新建总门禁。
旧审计正文：`docs/archive/route-reset-12c8628/docs/AUDIT_TODO.md`。

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
