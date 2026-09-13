# 产品功能路线

## M18 当前续接：执行者、长期运行与可维护性三部分（2026-09-13 第三轮）

本轮仍沿 M18：优先修真实证据丢失、不可恢复状态和错误语义终结，再接操作生命周期、资源与全成本。第三轮基于实际未提交树形成 14 项（4 P1＋10 P2），9 项有有界反例；报告不代表产品修复、远端 CI 或真实降本已验收。见 [第三轮报告](reviews/2026-09-13-executor-maintainability-audit/REPORT.md)。

三部分与顺序统一在 [NEXT_TASKS.md](NEXT_TASKS.md) 顶部：[A 上下文/GC](reviews/2026-09-13-executor-maintainability-audit/TASK_A_CONTEXT_GC.md)、[B 执行/恢复](reviews/2026-09-13-executor-maintainability-audit/TASK_B_EXECUTION_RECOVERY.md)、[C 成本/接入](reviews/2026-09-13-executor-maintainability-audit/TASK_C_COST_CONNECTIVITY.md)。首片 CTX-10 / EXEC-9 / COST-7 残余；已有 CTX/EXEC/COST 修复保留，COST-5 继续统一真实验收。

可维护性以同一规则能否贯穿实际路径衡量：复用正文 owner 解析、当前元数据合并、恢复根枚举、操作准入和真实压缩计划；不以更多词表或每入口一套补丁维持正确性。不新增通用调度器/第二状态库，不改变 Core 完成和副作用权威，不为缓存冻结当前事实。

## M18 第二轮路线记录（2026-09-12，已有成果保留）

长期稳定、高效可靠与同质量降本目标不变，不新立 M19。第一轮主体修复已进入未提交工作树；第二轮将“有界容器/局部测试”继续接成真实组合闭环。优先解决：完成任务窗口与 checkpoint 校验一致；可信恢复引用与 Unicode 安全；Live 必需证据在 GC/TTL/Pending/Stored 各位置的一致语义；慢 GC/保存的控制响应；总资源和全调用费用完整性。

三份任务和当前首片以 [NEXT_TASKS.md](NEXT_TASKS.md) 顶部为准：[A 上下文/GC](reviews/2026-09-12-gc-core-followup/TASK_A_CONTEXT_GC.md)、[B 执行核心/恢复](reviews/2026-09-12-gc-core-followup/TASK_B_EXECUTION_RECOVERY.md)、[C 缓存/成本](reviews/2026-09-12-gc-core-followup/TASK_C_COST_CACHE.md)。原 COST-5 保留为最后统一验收，权限、完成语义、证据新鲜性和冷恢复不因降本放宽。

当前修复并非全仓/真实 provider/长期资源已验收；事实边界见 [第二轮报告](reviews/2026-09-12-gc-core-followup/REPORT.md)。本轮只审查编排，不改实现。

## M18 第一轮路线记录（已有成果保留）

目标是在同样的产物质量、权限与恢复要求下，让执行者长期取得正确证据，保持取消/恢复和资源边界，并降低每个成功任务的全成本。三部分为 **A 上下文与长期记忆、B 执行核心与恢复、C 缓存与成本**；平台与 GUI 消费面并入相关功能切片。详细顺序见 [NEXT_TASKS.md](NEXT_TASKS.md) 顶部，依据见 [2026-09-12 审查](reviews/2026-09-12-executor-audit/REPORT.md)。

M17/9 月 11 日已有实现与未提交修改保留，尚缺的集成、真实产品与供应商验收按相应路径完成，不能因安排了 M18 就宣布 M17 正式通过。沿用 RuntimeActor、TaskAnchor/ExecutionState/Checkpoint、Core effect 权威和可替换 Context；不新增 Chronicle、TaskGraph、调度器或第二状态库，不以缓存收益冻结过期上下文。

首批 CTX-1 / EXEC-1 / COST-1 并行；随后落实约束/摘要正确性、完成任务和恢复引用有界、供应商计费字段与维护预算，最后复用既有评测做同起点真实长任务对照。未知账目不能当零，本地前缀或 cached ratio 不能当费用节省。

## 前序路线记录（已有成果与验收限制保留）

> 状态：**M17 收尾——可恢复的多入口工作台（N 系列，2026-09-08 切换）＋并行三线 A/B/C（2026-09-09 审查开线）。** M17 三线代码主体已落地（B1–B3、C0、P1、P2 主体、P3、G1–G3 客户端侧、E1）；2026-09-08 闭环审查（`11afdd7`）的 20 项链路断点（F01–F20）映射 N0–N8，N0–N3/N5/N6 已关闭，N4 主体落地待验收；2026-09-09 三线审查（`bbf7f5d`）把上下文/GC/搜索提为正式 A 线，与 B（Runtime/平台一致性）、C（桌面产品）并行推进，**N 系列主顺序不变**。CI 结论按 run 记录（见 [CURRENT.md](CURRENT.md)），不外推到任意 SHA。
> 旧报告中的"先全部可靠性收口、再 Chronicle/TaskGraph、再开发功能"不是当前执行顺序；Chronicle→TaskGraph→worker 也不是正式 GUI、公共应用接口或只读工具子 Agent 的技术前置。
> 不改变 Core 安全边界，不改写历史实验结果，不取消现有 CI。

## 目标与边界

长期目标：**可复用的本地 Runtime／Platform**；Coding Agent 与原生 GUI 是它的产品客户端。
用户给出一个仓库级任务，Agent 能维护短计划、查读修改、响应补充、有限执行、停止后续跑、冷恢复，并交付可审阅结果；同一套能力可通过非交互入口与正式桌面客户端使用。

默认范围：单用户、单工作区、一个活动任务焦点、一个 RuntimeActor、动态进程内 Context。
不是通用 Agent OS，也不是把任意自然语言需求自动证明正确。

已经发布的 v0.1.0、已有 M15/LT-EVAL 记录、doctor、检查点是基础，不再作为待重做事项。
**实现存在、测试通过、默认启用、真实任务跑通**分别记录。

<a id="route-to-a-usable-local-agent"></a>

## M17 收尾：可恢复的多入口工作台（N0–N8，当前阶段）

验收单位是用户动作而非组件数量：第二个客户端能连入；订阅后能看到一次真实工具结果；未知续跑结果不会自动重发；多行指令可提交；审批能查看具体对象；同一任务可从正式检查点重开；长会话不持续积累失效对象。已完成组件（Runtime StartWork、身份化监督、metadata 围栏、VerificationProbe 关联、MCP 适配器、GUI 外壳）不重做；断着的链路直接接通。批次与依赖见 [NEXT_TASKS.md](NEXT_TASKS.md)；审查原文 [reviews/2026-09-08-closure-audit-11afdd7/REPORT.md](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)。冷恢复/长期连接的正式声明等对应 N5/N1 通过。

**并行三线（2026-09-09 审查 `bbf7f5d` 开线）：**N 系列主顺序不变（N4 收尾验收 → N7 → N8），同时三线并行推进、互不干扰——**A 上下文/GC/搜索**（持久证据归属、片段覆盖如实、Rolling 摘要连续性，拥有 `context-simple`/`context-baselines`）、**B Runtime/平台一致性**（快照订阅同切点、关闭确认、共享 DTO 单一合入）、**C 桌面产品交付**（N4/N5 结果半/N7/N8 的 GUI 侧）。重叠切片一次执行、双边同时关闭；算法优化并入 A4，按真实瓶颈验收，不阻塞平台与 GUI。切片表、所有权与首批见 [NEXT_TASKS.md](NEXT_TASKS.md)「并行三线」节；审查原文 [reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)。

## M17：多入口平台与正式原生工作台（2026-09-07，代码主体落地）

三条工作线并行，按功能依赖控制交付，不用"基础全部清零"阻塞 GUI，也不让 GUI 自拼命令形成第二套业务逻辑：

```text
                 最小公共契约（C0）
                 /          \
      基础正确性修复        平台应用服务
      （B1/B2/B3）         （P1/P2/P3）
                                  │
                           正式原生 GUI
                           （G1/G2/G3）
                                  │
                       实际使用暴露接口缺口
                       （E1 扩展、R1 收口）
```

两个不同的"能开始"与"能声明完成"：GUI 布局、客户端库、只读状态、任务输入可与基础修复同时开始；涉及宿主执行、可靠清理、冷恢复的**正式支持声明**等待 B1/B2 对应验收。研究性优化不阻塞任何一条线。

首批开工：**B1、B2 直接开始；C0 文档切换已落地，DTO 约定完成后 P1、G1 同时推进。** 共享契约/`command.rs`/compose 入口单一维护者。

桌面端选型（2026-09-07 决定）：**.NET 10 LTS＋Avalonia＋独立 Rust 宿主＋本地 IPC（Windows Named Pipe／Linux UDS）**。边界：这是平台覆盖与工程交付取舍，不是已测出的资源占用最优；Avalonia 非 WebView 但也非逐控件原生包装；Linux 原生 Wayland 后端仍为实验 opt-in，发布时须明确发行版/显示后端/架构范围；SDK patch 实施时锁定。Windows-only 立场出现时才改选 WinUI 3。FFI 嵌入（Rust 编 DLL 交 C# 调）不采用：进程边界更贴合多入口与可替换 GUI，watchdog 的 `current_exe` re-entry 协议要求宿主是明确支持它的 Rust 可执行文件。

平台首组能力（不是导出全部 `RuntimeCommand`）：提交任务/追加输入/继续、取消/审批响应、快照/订阅、结果与差异按需读取、完整检查点恢复。状态接口第一版即解决衔接：一致快照＋游标→其后事件→缺口 `resync_required`；实时文字与耐久事件分开。对外提交带稳定逻辑提交键与受理回执；RPC 请求 ID 不自动等于跨重连/重启幂等。

完整工单与依赖见 [NEXT_TASKS.md](NEXT_TASKS.md)；阶段提案原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)。

## M16 顺序（已收口）

M16 替换此前 D0/F1–F6 活动队列。审查剩余是同一队列的前列，不是第二套待办。可执行步骤见 [NEXT_TASKS.md](NEXT_TASKS.md)。

| 切片 | 用户可见结果 | 本分支 |
|---|---|---|
| M16-00 活动路线 | 开发者只看到一条当前队列 | 本文档切换 |
| M16-01 继续与补充 | `/continue`；忙时输入可见处置 | 已关闭（2026-09-07）：走查转为 TUI 会话循环 E2E |
| M16-02 工作模式与完成语义 | `/work` `/plan`；待审阅 ≠ 已持久完成 | **已关闭（2026-09-07）**：待审阅/持久完成显式区分 |
| M16-03 有限执行与取消 | 模型轮预算让出后显式续跑；慢验证可取消 | 主体已落地；Provider 输出边界已关闭；Unix 硬崩溃监督见 PROCESS-01 |
| M16-04 冷恢复 | 验证过的检查点恢复同一任务并继续 | 已关闭（2026-09-07）：产品配置冷恢复走查落地 |
| M16-05 结果审阅 | `/review` 区分已知改动、真实检查、未验证 | 已关闭（2026-09-07）：双工作区走查转为无头 + TUI E2E |
| M16-06 上下文与搜索 | 互补片段不互冒覆盖；搜索不完整如实说明 | 主体已落地；CONTEXT-01 与 FIFO/句柄项已关闭 |
| M16-07 非交互入口 | 同一 Compose/RuntimeHandle 的 JSONL 调用 | N1/N2 已落地 |
| M16-08 试用收口 | 对应源码的包 + 三类真实任务记录 | 无头 live 已有；打包来源与 TUI 走查待做 |

M16-00–07 全部关闭（2026-09-07），走查均为自动化 E2E；PROCESS-01 仅剩 Linux CI 实证。剩余条件项：M16-08 真实 provider live 记录、PACKAGE-01 绑下次实际发布、MCP-01 绑默认启用。08 收口整个阶段，不阻止前面切片独立试用。

进程内 `/continue` 可以先用；可靠冷恢复、可取消宿主验证、发布包正确性，须在各自问题处置后才能宣称完成。

## 测试如何配合主线

开发循环用定向测试和短演示；集成复用现有跨平台 CI。相同源码、相同命令，没有新原因不反复重跑。
三个真实任务确认产品闭环，不声称普遍成功率。真实 provider 不可用写 `NOT_RUN`，不假绿，也不因此倒退去建基础设施。

## 核心资产：本阶段必做 vs 暂不扩展

- Context：资源版本与证据片段分开；最终渲染与消费 ACK 一致。不重写完整 Frame 编译器。
- GC：attention / semantic / residency 分离；语义终结有可信依据。不用缓存热度裁决真假。
- 搜索：精确定位、正确候选顺序、分页与扫描完整性分开。不上向量库。
- 调度：单 Actor；修真实阻塞的取消与清理。不新增通用 Scheduler。

算法候选（BM25、边际收益装配、SIEVE/TinyLFU）并入并行三线的 A4 切片，只在真实使用暴露对应瓶颈后验收，一次只改一个主机制。不以新算法胜出为任何平台/GUI 切片的前置。

<a id="post-m15-candidate-order"></a>
<a id="post-m15-candidate-order-proposal"></a>

## 不进入本阶段

Chronicle 数据库、结构化 TaskGraph、多 Agent、并行 worker、递归自修改、插件市场规模的生态扩展。
E1 只是**一个**受控真实能力与按需 Skill 的最小闭环，不是 MCP/插件生态建设。
需要更大规模时先指出现有简单路径解决不了的具体用户任务。

## 参考文档

[CURRENT.md](CURRENT.md) 记录事实；[AUDIT_TODO.md](AUDIT_TODO.md) 分流缺陷。
架构、执行、恢复和权限修改分别参考 [ARCHITECTURE.md](ARCHITECTURE.md)、[EXECUTION_MODEL.md](EXECUTION_MODEL.md)、[RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md)、[PLATFORM_SECURITY.md](PLATFORM_SECURITY.md)。
M16 提案原文：[reviews/2026-09-06-m16-proposal/](reviews/2026-09-06-m16-proposal/TRIAGE.md)；M17 续审原文：[reviews/2026-09-07-platform-native-audit/](reviews/2026-09-07-platform-native-audit/REPORT.md)。
