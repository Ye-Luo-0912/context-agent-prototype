# 当前工作

## 现在做什么

**当前阶段：M17——多入口平台与正式原生工作台（2026-09-07 切换）。** 三条工作线并行：基础正确性修复、平台应用服务、正式原生 GUI（.NET 10 LTS＋Avalonia＋独立 Rust 宿主＋本地 IPC）。首批开工：**B1、B2 直接开始；C0 的文档切换由本次更新落地，C0 剩余的协议 DTO 约定完成后 P1 与 G1 同时推进。**

M16-00–07 已全部关闭；M16-08 走查已落地（2026-09-07，见 [walkthroughs/2026-09-07-live-binary.md](walkthroughs/2026-09-07-live-binary.md)）。剩余条件项不变：真实 provider live 记录（不可用写 `NOT_RUN`）、下次实际发布的 PACKAGE-01、默认启用 MCP 后的 MCP-01。

2026-09-06 深入续审的代码项保持关闭；但 2026-09-07 外部续审（基线 `b299c6a`，部分源码静态审查）指出其中两块的**关闭范围只覆盖已落地实现，不覆盖整个保证**：监督台账（PROCESS-01 引入）缺进程创建身份与清理确认；STORAGE-02 的修复没有消除 helper 内部 rename 之后的目录同步失败窗口。这些残余按证据等级进入 M17 前列，不重开 M16，也不推翻已落地的 Windows 围栏、Unix 看门狗与台账本身。

原文与任务包：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)；
可执行工单全文：[reviews/2026-09-07-platform-native-audit/NEXT_STAGE_TASKS.md](reviews/2026-09-07-platform-native-audit/NEXT_STAGE_TASKS.md)。
执行队列：[NEXT_TASKS.md](NEXT_TASKS.md)。

## 2026-09-07 续审残余（M17 前列）

审查固定 `b299c6a08fdb055a0148a24dea65053861df6ff1`（部分源码：19 路径正文读取、4 个完整返回；无本地工具链，未运行构建/测试；远端 CI 六 job 成功是远端事实）。F01–F09 的关键代码定位（台账字段、`ProcessRunTool::new` 默认值、watchdog 判活与 `Drop`、metadata rename 后同步、`anchor_revision` 跨任务 `max`、headless 文字推断审批、`work.rs` 提交序列、`--prompt=-` 全量读入）已于 2026-09-07 在本工作树 HEAD `7c3236d` 静态复核成立。全部为静态/条件性结论，未做真实 PID 复用误杀、故障注入或 GUI 实测。

| 发现 | 一句话事实 | 归属工单 |
|---|---|---|
| F01/F02 | 监督台账只存 `pid+purpose`；`process_is_running` 后直接杀；写失败忽略、读失败当空、kill 后未确认即删记录；`ChildLease::Drop` 无条件释放。`lifecycle.rs` 已有 `ProcessIdentity` 未被复用 | B1（主体已关闭 2026-09-07：身份化台账、类型化对账门、清理回执；扩展验收项随 P3） |
| F03 | `RecipeProofRunner::new` 自建 `ProcessRunTool::new`（默认 `host_death_watchdog=false`）；compose 未把普通 dispatcher 的监督配置贯通到宿主 proof 路径 | B1（已关闭：compose `host_death_watchdog` 统一注入两条车道） |
| F04 | watchdog 以 `kill(leader,0)` 判活，组长被 reap 后同组成员可能漏杀（OS 探针已证机制）；`Drop` 中同步 `child.wait()` 无期限 | B1（已关闭：组内成员扫描＋有界 Drop reap，真 Linux 验证） |
| F05 | `persist_authority_metadata` 在 rename 发布之后 `sync_directory(parent)?` 失败仍返回 Err；`compact_locked` 在换 writer 前被 `?` 中断，可能磁盘新代、内存旧代 | B2（已关闭 2026-09-07：RecoveryRequired＋writer 围栏＋注入测试） |
| F06 | `StatusProjection` 切换任务不重置 `anchor_revision` 后又跨任务 `max`；headless 用输出文字包含 `denied by approval policy` 判审批拒绝；终态区分不足 | P2（revision-per-task 修复已在工作树进行中） |
| F07 | `work.rs` 的 set_focus→list_tasks→replace→user_message 是多次独立 await，`UserMessage` 不绑定任务身份，多客户端交错可误投 | P1（工作树实现中） |
| F08 | `queue_error_verifications` 以输出实体匹配所有 live Error，无故障/任务/覆盖域/版本关联 | B3（主体已关闭 2026-09-07：同配方 recipe_id 关联终结；recipe 版本/覆盖身份与任务级关联仍开放） |
| F09 | `--prompt=-` 先 `read_to_string` 全量读入；grant 文件 stat 后整体读取；headless 同步写 stdout/文件不受事件等待超时约束 | B3（主体已关闭 2026-09-07：读入时计费＋有界输出 sink；无期限 stdin 读取期限仍开放） |

不在以上范围、同样成立的事实：平台认证 adapter 目前只处理 operation query/cancel（work/approval 路由已由 P1/P2 与 `crates/agent-host` 补齐，2026-09-07）；headless JSONL 过滤实时 delta。事件 wire 契约仍是 P2 的输入，不是新缺陷结论。

## 三线并行的执行原则

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
