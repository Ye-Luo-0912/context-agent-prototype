# 当前工作

## 现在做什么

**当前阶段：M17 收尾——可恢复的多入口工作台（N 系列，2026-09-08 切换）。** M17 三线的代码主体已落地（B1–B3、C0、P1、P2 主体、P3 宿主、G1–G3 客户端侧、E1；对照见 [NEXT_TASKS.md](NEXT_TASKS.md) 的 M17 队列表）。本轮的判断变了：**主要缺口不是缺组件，而是端到端链路断着**——订阅成功但事件 receiver 被丢弃、重连自动重发修改操作、宿主恢复绕过正式信封解码、多连接/会话释放/停机缺口。2026-09-08 外部闭环审查（基线 `11afdd747d6cbbb58ef0d7371841e8e365c4f8db`）的 20 项发现（F01–F20）映射为 N0–N8。

**N0 已关闭（2026-09-08）：CI run `34163939549` 七 job 全绿**——fmt 修复、宿主 Linux cfg＋UDS e2e、.NET job 入 CI、F20 测试修准；修复链顺带消掉了 conformance 角色准入、protocol 夹具 lint、agent-replay 探针夹具（契约演进被 fmt 红遮蔽四轮的实证）、supervision 锁退避（两进程同 workspace 启动的真实生产问题）。第一批 **N1/N2/N6 已关闭并合入 main**（CI run `34173331100` 七 job 全绿）：宿主长期服务、客户端操作安全、核心语义边界三线由并行代理实现、主会话按 A→B→C 集成验证（host e2e 双平台 / dotnet 35 / context-simple 302 / workspace 全量零失败）。第二批 **N3/N5 已关闭并合入 main**（CI run `34268863699` 七 job 全绿）：真实事件从 Runtime 经宿主到客户端全链贯通、多行输入合法、正式信封恢复。剩余：**N4 正式 GUI 操作面**（消费真实事件流＋知情审批＋稳定行生命周期）→ N7 长会话；N8 能力配置与来源绑定发布（含 PACKAGE-01）。

审查原文：[reviews/2026-09-08-closure-audit-11afdd7/REPORT.md](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)；
工单全文：[reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md](reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md)。
执行队列：[NEXT_TASKS.md](NEXT_TASKS.md)。

## 2026-09-08 闭环审查（F01–F20 → N 系列）

审查为部分源码静态审查（55 路径正文；apps/Agent.Desktop 10 文件、clients/dotnet 19 文件、agent-host 5 文件全文；克隆仍因 DNS 失败，无本地工具链，未运行构建/测试）。F 系列关键定位（subscribe 丢弃 receiver、FIRST_PIPE_INSTANCE、无条件 remove_file、会话不 revoke、裸 JSON 恢复、validate_text 拒换行、AsyncCommandGroup 积累、Resident/Warm lease 差异、to_summaries 全量投影）已于 2026-09-08 在本工作树 HEAD `11afdd7` 静态复核成立。逐条细节与不要做什么见 [AUDIT_TODO.md](AUDIT_TODO.md)。

**先确认已修好的旧问题（不原地重做）**：监督台账身份/类型化对账/确认式清理；proof runner 监督接线；metadata 发布不确定围栏；原子 `StartWork`＋进程内受理台账；同任务同 `VerificationProbe` 验证关联；`skill_read`＋MCP/Plugin 配置缝。均为定向静态确认，不升级为全平台运行验收。

**链路断点按 N 归属**：事件转发断（F06/F11→N3）；宿主长期服务断（F02–F05→N1）；修改重发与连接终态（F07–F09/F19→N2）；正式信封恢复（F10→N5）；知情审批与对象生命周期（F12/F13→N4/N7）；决策误终结/lease 跨层/Skill 句柄/catalog 投影（F15–F18→N6）；测试名与路径不符（F20→N0/N8）。

上一轮（2026-09-07，基线 `b299c6a`）的 F01–F09 残余表已由 M17 工单消化：B1/B2 关闭，B3/P1/P2 主体关闭（recipe 版本/覆盖身份关联、无期限 stdin 读取期限两项残余随 N6/N3 收口）。原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)。

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
