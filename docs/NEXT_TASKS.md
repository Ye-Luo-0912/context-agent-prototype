# 可执行任务队列

> 状态：**M17 收尾——可恢复的多入口工作台（N 系列，2026-09-08 切换）。** 上一队列（M17 三线）的代码主体已落地：B1–B3、C0、P1、P2 主体、P3 宿主、G1–G3 客户端侧、E1 全部关闭或落地（见下方 M17 队列表）。2026-09-08 外部闭环审查（基线 `11afdd747d6cbbb58ef0d7371841e8e365c4f8db`，55 路径正文、新 GUI/客户端/宿主三子树全文）指出：**组件存在 ≠ 链路接通**——事件订阅 receiver 被丢弃、重连自动重发修改操作、宿主恢复绕过正式信封解码、多连接/会话释放/停机缺口等 20 项（F01–F20），进入本队列。
> **CI 状态按 run 记录，不外推到任意 SHA**（2026-09-09 核对）：run `34163939549`（`96e4605`）与 `34268863699`（N3/N5 批次）七 job 全绿；run `34271105841`（`bbf7f5d`）Windows 全测试失败于 `context-simple` admit 并发测试（其余 job 通过），`7e026ee` 已改为比例断言修准 load-flaky（确定性屏障方案仍为 A 线建议项）；N4 批次 run `34278244036`/`34278810636` 文档写作时进行中。更早的 fmt 红与 conformance/protocol/replay/supervision 四个被遮蔽问题的修复记录见 git 历史。
> 本队列接续 M17 未闭环项；不重做已落地的 B1/B2/C0/P1/G1/E1，也不新增 Chronicle/TaskGraph/第二套状态权威。
> 剩余条件项不变：真实 provider live（无凭据写 `NOT_RUN`）、下次实际发布的 PACKAGE-01（并入 N8）、默认启用 MCP 后的 MCP-01（E1 已覆盖声明车道的取消贯通）。
> 审查原文：[reviews/2026-09-08-closure-audit-11afdd7/REPORT.md](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)；工单全文：[reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md](reviews/2026-09-08-closure-audit-11afdd7/NEXT_STAGE_TASKS.md)。
> 2026-09-09 三线审查（基线 `bbf7f5d`）：[reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)；由此开出的并行三线（A/B/C）切片与不干扰规则见下方「并行三线」节——**N 系列主顺序不变，三线与之并行执行**。
> 上一轮（2026-09-07 platform-native）原文：[reviews/2026-09-07-platform-native-audit/REPORT.md](reviews/2026-09-07-platform-native-audit/REPORT.md)。
> 不替代 Core、Effect、Workspace、恢复与输出边界契约；不改写历史评测结论。

## 开始执行

只读 [CURRENT.md](CURRENT.md) 和本表当前工单，然后读该工单的实现、调用者和测试。
已完成的项定向确认后跳过。一次只做一个工单。
**四个不同事实**：代码存在、已接真实传输/产品入口、检查实际执行、真实用户场景跑通——分别记录，不互相冒充。
执行任何工单前先 `git status --short` / `git rev-parse HEAD`；审查未读过的模块现场补读，不宣称全仓已审。

## 并行与进入顺序

| 批次 | 工单 | 约束 |
|---|---|---|
| 第一批 | **N0 构建与验证入口**（立即）；N1 宿主长期服务、N2 客户端操作安全、N6 核心语义与读取边界 | 互不阻塞；N0 的集成检查期间即可并行开工 |
| 第二批 | N3 真实事件与完整输入 → N4 正式 GUI 操作链；N5 恢复与结果审阅 | N3 最小契约固定后 GUI 接真实数据 |
| 第三批 | N7 长会话低占用；N8 能力配置与联合交付（含 PACKAGE-01） | 高风险恢复/执行声明等对应修复通过 |

共享协议、`RuntimeCommand`、compose 入口单一维护者；修改结果未知时不重发（返回 Unknown/要求重同步）；快照与事件以同一 run/host 身份衔接；客户端不从显示文字制造完成、审批或恢复事实。

2026-09-09 起三线（A/B/C）与 N 系列同步执行、互不阻塞：**N 系列保持原顺序（N4 收尾验收 → N7 → N8），三线切片并行推进**；重叠切片一次执行、两边同时关闭。所有权与不干扰规则见下方「并行三线」节。

## 当前队列（M17 收尾：N 系列）

| 顺序 | 工单 | 线 | 交付物 | 状态 | 依赖 |
|---|---|---|---|---|---|
| 1 | ~~N0~~ | 集成 | fmt/cfg 修复＋宿主 Linux 构建＋.NET 入 CI＋测试修准 | 已关闭（2026-09-08）：CI run `34163939549` 七 job 全绿；另修 conformance 角色准入、protocol 夹具 lint、replay 探针夹具、supervision 锁退避 | 无 |
| 2 | ~~N1~~ | 平台 | 宿主多连接、会话释放、端点所有权、可靠停机 | 已关闭（2026-09-08）：连接归属 grant＋revoke、65 次重连回归、有界停机、fail-closed UDS；CI run `34173331100` 全绿 | N0 ✓ |
| 3 | ~~N2~~ | 客户端 | 未知修改不重发、连接终态不复活、single-flight | 已关闭（2026-09-08）：修改只发一次（Unknown 语义）、终态故障路径＋半帧毒化、single-flight＋代际、双沿验证；dotnet 35/35 | N0 ✓ |
| 4 | ~~N3~~ | 契约 | 事件 receiver 保留到连接、notification 验证、多行正文 | 已关闭（2026-09-08）：契约放行多行＋字节上限（`76ef359`）；宿主保留 receiver＋watermark 切点＋同步管道轮询修复（`43a198b`/`184ace4`）；客户端按 kind 分派＋有界事件流（C3 两提交）；CI run `34268863699` 全绿 | N1 ✓, N2 ✓ |
| 5 | N4 | GUI | 计划/输出/知情审批/取消/继续/真实状态 | **主体已落地**（2026-09-09，`9fb2030`/`433d21e`/`843803f`）：知情审批快照（gate 风险＋有界目标摘要）＋桌面稳定行＋真实事件消费＋真实宿主默认传输；dotnet 56/56（提交记录）；**已关闭（2026-09-08/09）：CI run `34278810636` 七 job 全绿**（=三线 C1/C2 主体） | N3 ✓ |
| 6 | N5 | 平台/GUI | 正式信封恢复＋结果/差异/工件按需读取 | 恢复半已关闭（2026-09-08，`808773c`，CI `34268863699` 验证）：信封解码＋可验证 latest＋3 e2e；**backlog**：Rolling 引擎不跟踪 focus，活动任务检查点 fail-closed（需动 context-baselines）；结果/差异/工件读取仍开放 | N2 ✓, N3 ✓ |
| 7 | ~~N6~~ | 基础 | 决策不误终结、lease 跨层一致、Skill 受限句柄、catalog 有界 | 已关闭（2026-09-08）：F15 决策需证明、F16 跨层到期保护、F17 包内普通文件围栏、F18 惰性投影；context-simple 302/302 | N0 ✓ |
| 8 | ~~N7~~ | GUI/测量 | 对象与文本保留有界、指标覆盖如实 | **已关闭（2026-09-09，`87850ae`＋C4 记录）**：MetricsSession 覆盖标注 root_only/full_tree/unknown（Windows 不冒充 whole-tree）＋有界采样环＋显式 idle 标记＋DeltaCoalescer 定时刷新（短 delta 后无输入也按间隔刷新）；输出行/字节双界（N4 已落）＋ViewModel 关闭释放 coalescer；MetricsSession/DeltaCoalescer 测试 9/9、dotnet 全量 72/72、桌面构建 0 错误 | N4 |
| 9 | N8 | 扩展/交付 | MCP/Plugin 可配置使用＋Rust/.NET 来源绑定发布（并入 PACKAGE-01、原 R1） | **部分落地（2026-09-09）**：PACKAGE-01 来源绑定打包（`3352273`：--target-dir 构建与复制同身份、干净 staging、agent-host/desktop 入包、SOURCE.txt、递归 SHA256SUMS、PS 原生退出码；Windows 端到端验证通过）；.NET→宿主→Runtime→工具→事件→GUI 全链 e2e（`aeddfbd` HostChainTests，真实宿主二进制＋demo model，本机 1/1、CI dotnet job 已接宿主构建）；**B4 宿主受限 MCP/Plugin 配置路径由 B 线代理并行执行中**（config.rs 在途） | N1–N6 |
| 10 | ~~PACKAGE-01~~ | 条件 | 打包来源绑定 | **已并入 N8 关闭（2026-09-09，`3352273`）**：Windows 端到端打包验证通过；Linux 侧由 CI package job 复核 | — |
| 11 | MCP-01 | 条件 | MCP 写/连接/读可取消 | E1 已覆盖声明车道；新声明路径触发时补 | — |

阶段后候选（不作为本阶段前置）：只读工具子 Agent（独立状态、有限预算、无递归）；Context/GC/搜索的算法优化并入 A4，按真实瓶颈验收，不单独立项、不阻塞 B/C。

---

## 并行三线（2026-09-09 审查 `bbf7f5d`；与 N 系列同步执行、互不干扰）

来源：2026-09-09 外部三线审查（基线 `bbf7f5d3080747fe113a4a07469ac2fd4ccf2d35`；静态审查＋远端 CI 观察＋隔离探针，无完整 checkout、无工具链，20 crate 目录树核对、28 路径重点阅读）——[reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md](reviews/2026-09-09-audit-bbf7f5d-three-tracks/REPORT.md)。缺陷明细与「不要做什么」见 [AUDIT_TODO.md](AUDIT_TODO.md) 的 2026-09-09 表；标注「已复核」的定位已于 2026-09-09 在本工作树 HEAD `7e026ee` 静态确认。

**执行关系：N 系列主顺序不变（N4 收尾验收 → N7 → N8）；三线切片与之并行推进，互不阻塞。重叠切片一次执行、双方同时关闭，不重复立项、不拆第二套待办。**

**所有权与不干扰规则：**
- **A 线**拥有 `context-simple`、`context-baselines`；B/C 不改这两个 crate 的语义路径。
- **B 线**拥有 `agent-host`、`agent-platform-protocol`、`clients/dotnet` 与共享 DTO；`agent-contracts`/`RuntimeCommand`/compose 入口仍单一维护者（B 线合入），A 提证据/恢复字段需求、C 提界面实际需求，不各自发明 DTO。
- **C 线**拥有 `apps/Agent.Desktop` 界面与 ViewModel 生命周期。
- 每线内部按切片串行（A1→A2→A3→A4 等）；三线不在同一文件上互相覆盖，跨线需求走接口请求。

### A — 上下文、GC、搜索

| 切片 | 交付 | 对应缺陷（AUDIT_TODO 2026-09-09 表） | 与 N 系列关系 |
|---|---|---|---|
| A1 | **已落地（`6f47a90` 修复＋`df5b972` 测试，随 N4 批次合入）**：GC recall 不先删 blob（持久归属）、GC 外置取消安全、隔离失败不丢 owner、拒绝准入无副作用 | GC-DEL / GC-CANCEL / QUARANTINE / ADMIT-TERMINAL —— 均已落地 | 附带 ADMIT-TEST 确定性屏障同批落地（`6f47a90`，替代比例断言）；context-simple 全量绿 |
| A2 | **已落地（2026-09-09）**：截断后行范围/partial/required 传播一致、最终曝光如实 | RANGE-PARTIAL —— 已落地 | — |
| A3 | **已落地（2026-09-09）**：Rolling 折叠输入包含旧摘要、默认 profile 恢复（focus 跟踪）、显式准入使用期 | ROLLING-PRIOR / ROLLING-FOCUS / ADMIT-LEASE —— 均已落地 | ROLLING-FOCUS 即 N5 backlog 项（Rolling 不跟踪 focus）——代码缺口已关闭，正式冷恢复声明仍等 N5 验收 |
| A4 | 查询预处理复用、候选相关性进最终排序、边际预算装配、维护成本预算 | 报告第七节设计建议，非缺陷 | 算法优化按真实瓶颈验收，不阻塞 B/C |

**用户结果：**长期任务中的旧线索能找回；第二次压缩不会无意抹掉前次摘要；GC 不把 RAM 迁移误当持久保存。

### B — Runtime 与平台一致性

| 切片 | 交付 | 对应缺陷 | 与 N 系列关系 |
|---|---|---|---|
| B1 | 快照/订阅同一切点、live 与 durable 分流、重连代际/epoch 边界 | SNAP-GAP / LIVE-DELTA（两项均**已落地 2026-09-09**，见 AUDIT_TODO 注记） | N3 已接通链路，B1 修一致性残余——**已关闭**（agent-host 单测＋e2e 7/7、agent-contracts 163、dotnet 客户端测试全绿；无 wire 变更） |
| B2 | 队列 Completion 语义、宿主「取消全部」与「确认结束」分离、服务失败收口、retyped 原始信封验证 | QUEUE-COMPLETION / CANCEL-ALL / RETYPED（三项均**已落地 2026-09-09**，见 AUDIT_TODO 注记；agent-host 单测 5＋e2e 8/8、dotnet 72/72——计数含并行线当日新增） | N1 可靠停机的收口延伸——**代码落地，CI 全量确认后关闭** |
| B3 | GUI 所需真实任务/审批详情/结果/工件/只读 Context 接口 | — | 即 N5 结果半（结果/差异/工件按需读取） |
| B4 | 同一正式宿主 profile 多入口复用、已有 MCP/Skill 配置接入 | — | 即 N8 |

**用户结果：**重连不漏中间状态；坏连接与退出有明确结局；多入口调用同一套应用行为。

### C — 正式原生桌面与产品交付

| 切片 | 交付 | 对应缺陷 | 与 N 系列关系 |
|---|---|---|---|
| C1 | 真实事件消费者、模型/工具输出、真实计划与状态 | GUI-EVENTS（基线时点；`843803f` 已落地，随 N4 验收） | 即 N4 主体 |
| C2 | 知情审批、稳定行对象、单次刷新、连接代际与关闭清理 | F12/F13（`9fb2030`/`433d21e`/`843803f` 已落地，随 N4 验收） | 即 N4 主体＋N7 前半 |
| C3 | 修改审阅、正式冷恢复走查、工件按需读取、只读 Context 检查 | — | 即 N5 结果半（GUI 侧）；冷恢复正式声明等 A3/B 对应验收。**已落地（2026-09-09，`391f121`）：未知提交按快照事实解除＋restore-walkthrough 走查测试（scripted host 上的连接丢失/重启/重建语义）；修改审阅、工件按需读取、只读 Context 接线依赖 B3 结果/工件读取接口（B 线在途）** |
| C4 | 长会话资源上界、准确测量、Rust＋.NET 来源绑定包 | F14 | 即 N7＋N8。**已落地（2026-09-09）：N7 `87850ae`（MetricsSession 覆盖标注/有界采样环/显式 idle/时间预算 coalescer）＋N8 打包 `3352273`（dist.sh/dist.ps1 来源绑定：--target-dir 构建、干净 staging、agent-host/desktop 入包、SOURCE.txt 来源身份、递归 SHA256SUMS、PowerShell 原生退出码；Windows 端到端打包验证通过，bash 语法复核）** |

**用户结果：**正式客户端能提交、观察、审批、继续、恢复和审阅，不依赖布局夹具。

### 首批与依赖

- **A1–A3 已落地**（A1：`6f47a90`＋`df5b972`，随 N4 批次合入；A2/A3：2026-09-09 本批提交，context-simple 311＋context-baselines 11 全绿，host_restore 在 A 提交树 3/3）；B1/B2 已关闭。A4 为报告第七节设计建议（非缺陷，按真实瓶颈验收，不阻塞 B/C）；C1/C2 主体已随 N4 落地，其剩余（真实计划/open-loops 投影，当前诚实显示「不可用」）依赖 B3 的快照字段；C3/C4 分别接 N5 结果半与 N7/N8。
- C 的事件消费与对象生命周期不等 A 的算法实验；但**冷恢复、证据完整性等正式支持声明，必须等对应 A/B 回归通过**（沿用既有 B1/B2 声明门槛原则）。
- 每个切片回执照旧：改了什么、接进哪个真实用户动作、实际跑了什么、还有什么没验证、下一步是什么。

---

## N0 — 恢复构建与验证入口（已关闭 2026-09-08，CI run `34163939549` 全绿）

**用户结果：** 支持平台（Linux/Windows）的宿主与客户端重新可被 CI 真实验证；测试名与实际验证路径一致。

**事实：** CI run `34148921895` 在两个平台均停于 `cargo fmt --check`（违规集中在 `crates/agent-host`）；`HostServer::serve` 的 NamedPipe 分支无条件引用 `#[cfg(windows)]` 的 `winpipe` 模块（`lib.rs:234` vs `:503`），Linux 宿主构建存在静态缺口；host_e2e 的 Unix 连接辅助无真实 UDS。

**步骤：** `cargo fmt --all`（仅必要范围）；为两个平台提供明确 cfg 分支或受支持/不支持实现；现有 CI 增补 agent-host Linux 分片与 .NET build/test；修准 F20 列出的现有测试（半帧样本应为合法长度前缀、同连接乱序、原 key 重试、服务线程错误必须检查）。

**检查：** `cargo fmt --all -- --check`；`cargo check -p agent-host --all-targets --locked`（Linux＋Windows）；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`；`dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`。

**做到这里停止：** 不新建评测框架；不等全仓人工复测完成才开 N1/N2/N6。

## N1 — 宿主可长期接入、可安全关闭

**用户结果：** 第二个客户端能连入；反复重连不耗尽会话表；Ctrl-C 有界退出；端点不被误删/误抢。

**已复核事实：** `winpipe.rs:186` 每个管道实例都带 `FILE_FLAG_FIRST_PIPE_INSTANCE`（第二实例创建失败→accept 退出）；连接退出只 drop router 不 revoke session（64 上限 `.expect`，第 65 次顺序连接 panic）；accept 循环无停止通道（Ctrl-C 后 join 可能永久阻塞）；`lib.rs:187/251` bind 前无条件 `remove_file`（可删普通文件/解除他人 listener；默认 `/tmp` 固定名跨工作区冲突）。

**步骤：** 首实例独占语义仅用于名称占用检查，后续实例正常模式＋RAII 句柄；session grant 归属连接 guard，全部退出路径 revoke，install 错误受控拒绝；显式停止信号＋连接集合关闭＋服务失败回执，有界 join；UDS 用户私有、按工作区区分的端点，只清理可证明属于自己的。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；两并行客户端＋≥65 次顺序重连授权表回落；无客户端/半帧/慢读客户端下有界停机；普通文件占端点拒绝且不删除。

**做到这里停止：** 不做公网 TCP/HTTP、系统服务安装器、多工作区调度。

## N2 — 未知修改结果不重发，连接状态不复活

**用户结果：** continue 已被接受但回复丢失时不会自动续跑第二段；坏连接不被复用；并发连接请求只建一条连接。

**已复核事实：** `ResumableSession.RunAsync` 捕获连接异常后重连并重试 operation，submit/continue/cancel 全走它（审批答复已正确排除）；continue/cancel 正文不绑定预期 task/turn/generation；`Fault()` 只失败 pending 不关流不标终态；半帧写入失败不毒化连接；`LiveAsync` 连接/握手在锁外、无 single-flight 与代际约束。

**步骤：** 查询与修改重试策略分离，未知修改结果返回 Unknown 并要求查询/重同步；修改携带预期 task/turn/generation＋宿主 incarnation；single-flight connect＋handshake 成功才安装＋Dispose 防迟到复活；Fault 单一终态路径（关流、拒新请求、结清 waiter）；每个类型化 API 在实际发送/接受路径运行 payload 验证（F19）。

**检查（建议，未执行）：** `dotnet test …Agent.Client.Tests.csproj --filter "ConnectionTests|ResumableSessionTests"`；`cargo test -p agent-platform-protocol work`。

**做到这里停止：** 不建通用持久幂等数据库；无法证明时返回 Unknown，不猜测。

## N3 — 同一连接上的事件、快照和完整输入

**用户结果：** 订阅后真实收到受理/工具/助手/终态事件；多行开发要求可提交。

**已复核事实：** `agent-host/src/lib.rs:425` `work.subscribe` 握手成功后 `Ok((response, _receiver))` 丢弃事件 receiver；客户端 `Dispatch` 在分型前要求 `request_id`（合法 notification 被拒）；`work.rs:465` `validate_text` 拒绝一切控制字符（含 LF/TAB）而 GUI 文本框 AcceptsReturn=true。

**步骤：** 宿主保留 receiver 到连接关闭，响应/通知共用单一有界 writer；客户端按 kind 验证（notification 无需 request_id）；snapshot+subscribe 一致切点（沿用 barrier/resync-only，缺口返回 resync_required）；短标题与有界完整正文分离，正文允许合法换行/制表，身份/路径仍严格。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；`cargo test -p agent-platform-protocol work`；`dotnet test …Agent.Client.Tests.csproj`；中文多行/emoji 提交、慢消费者触发明确 gap/resync。

**做到这里停止：** 不新建 Chronicle；不为跨语言更换长度前缀传输。

## N4 — 从按钮和快照变成真正的任务操作面（当前工单：主体已落地，验收待 CI）

**用户结果：** 提交→工具/输出→知情审批→让出/取消→继续→产出待审是一条正式链；待审批反复刷新不积累命令对象。

**事实（审查基线 `bbf7f5d` 时点）：** 待审批快照只有 request_id＋call_name（无路径/argv/参数/风险）；每 3 秒刷新重建审批行、每行两个命令加入长寿命 `AsyncCommandGroup` 不移除（推导：1 小时≈2400 引用，非实测）；桌面默认 FixtureLayout。

**已落地（2026-09-09，`9fb2030`/`433d21e`/`843803f`，提交记录）：** ① 知情审批快照（F12）：`pending_approvals` 携带 gate 类型化风险（`ApprovalRisk`，wire snake_case）＋256 字符有界操作员目标摘要（投影自 path/files[].path/command/argv 等结构化参数，缺失为 None→UI 显示「不可用」，不从文字推断）；.NET 镜像 DTO fail-closed 解码（risk 缺失即拒收）＋共享端点推导 fixture。② 稳定行生命周期（F13）：审批行按 request_id 复用、移除撤销 `AsyncCommandGroup` 注册（200 次刷新演练计数恒定）、刷新 single-flight＋连接代际否决迟到结果、窗口关闭取消单一 lifetime。③ 真实事件消费（N3 API）：默认通道接真实宿主（fixture 降为显式「布局预览（非执行器）」）；单一后台消费者读 `IAgentConnection.Events`，UI 线程渲染类型化事实（模型增量过 DeltaCoalescer），3 秒轮询降为 10 秒兜底；输出 400 行＋64 KiB 双界限整行淘汰。④ 诚实状态：运行态仅由类型化快照布尔渲染；计划/open-loops 缺字段时显示「不可用」；未知提交结果保留 client_request_id 幂等重试，审批答复不自动重试。测试：WorkbenchLifecycleTests＋WorkbenchIntegrationTests（ScriptedEventHost 上 submit 回执→工具事件→审批详情→Delivered→终态），dotnet 56/56、桌面构建 0 错误（提交记录）。

**验收待确认：** CI run `34278244036`/`34278810636`（文档写作时进行中）；真实计划/open-loops/结果卡投影等 B3 快照字段（当前诚实显示「不可用」）；真实 provider 场景照旧 `NOT_RUN`。

**检查：** 已执行（提交记录）：`dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj` 56/56；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。CI 全量确认后关闭本工单（同时关闭三线 C1/C2 主体）。

**做到这里停止：** 不做 IDE/编辑器；GUI 不保存第二份任务/权限/完成真相。

## N5 — 正式检查点恢复与可审阅工件

**用户结果：** 同一正式宿主保存→关闭→重启→恢复原任务并显式继续；结果/差异/工件按需可读。

**已复核事实：** `agent-host/src/main.rs:207-213` `--restore-latest` 按文件名枚举 JSON 后直接 `serde_json::from_str::<RuntimeCheckpoint>`，绕过 CheckpointStore 的版本/checksum 信封（`decode_checkpoint_file/bytes` 已存在未用）。

**步骤：** 统一走 CheckpointStore 受限验证解码＋完整 `RuntimeInstance.restore`；profile 来源与身份显示明确；变更/工件经现有事实读取，只读调用不新建模型回合；GUI 区分断开窗口/取消任务/停止宿主/恢复继续。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；`cargo test -p agent-compose`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。

**做到这里停止：** 不改检查点格式、不建第二恢复引擎。

## N6 — 修准语义保留、只读成本与 Skill 路径

**用户结果：** 同文件两条兼容决策共存；有效 lease 不因正文位置改变终态；Skill 读取不能经 symlink/FIFO 越出包外；catalog limit=0 不做全量投影。

**已复核事实：** 决策 supersession 仍按实体/子串重合排队 `Superseded`（无同任务/决策键/显式替代约束）；`residency.rs:315-316` 的 Warm 路径检查 keep_alive/lease 而 Resident TTL 路径（`gc/minor.rs`）无此检查；`plugin.rs` `skill_read` 词法相对检查后普通 `File::open`（包内 symlink 可指向包外，探针已证机制；FIFO 可在 take 生效前阻塞）；`engine.rs:1705/1710` `to_summaries` 先全量投影再 `bounded_catalog`。

**步骤：** 实体匹配降为相关性，仅明确替代目标＋正确任务范围才进终态；提取跨层共用到期保护（lease/keep_alive 范围明确，终态不可复活）；catalog 早退 limit0＋惰性投影/选中后复制；Skill 复用既有 ConfinedDir/受限普通文件句柄。拆小 PR。

**检查（建议，未执行）：** `cargo test -p context-simple`；`cargo test -p agent-runtime --lib plugin::`。

**做到这里停止：** 不加向量/学习排序/新 GC 策略；可与 N1/N2 并行。

## N7 — 长会话有界而且指标可解释

**用户结果：** 长会话命令、pending、事件、文本、采样缓存有界回落；指标覆盖范围如实标注。

**事实：** MetricsSession Windows 无法枚举 parent 仍报 whole-tree、Linux 遍历提前标记 seen 可能漏孙进程、末样本直接标 idle、`_samples` 无限追加；DeltaCoalescer 仅 Append 时检查时间；输出 400 行限制不等于字节界限。

**步骤：** 输出按 chars/bytes/rows 共同限额、增量显示；采样覆盖标 root_only/full_tree/unknown、有界环；关闭时挂接 ViewModel 异步清理。

**检查（建议，未执行）：** `dotnet test …Agent.Client.Tests.csproj --filter "MetricsSessionTests|DeltaCoalescerTests"`；`dotnet build … -c Release`。

**做到这里停止：** 不把 AOT/零拷贝列为发布前置；不新增指标数据库。

## N8 — 把现有能力接入产品并完成来源绑定发布

**用户结果：** 安装后连接真实宿主而非 fixture；MCP/Skill 经目录与权限边界可配置使用；包来源可追溯。

**步骤：** 宿主暴露受限 MCP/Plugin 配置路径与 supported/unsupported；补 .NET→Rust host→Runtime→Tool→Event→GUI 真实用例（scripted model）；打包含宿主/桌面/依赖，干净 staging＋来源身份（即 PACKAGE-01/原 R1 范围，届时一并关闭）；三类真实任务记录，未执行写 NOT_RUN。

**检查（建议，未执行）：** `cargo test -p agent-host --test host_e2e`；`cargo test -p agent-capability-process`；`dotnet build … -c Release`。

**做到这里停止：** 不新建 release/tag 除非明确要求；本切片验收前不写 M17 已完成。

---

## M17 队列（主体落地，2026-09-07；闭环残余由上方 N 系列接手）

| 顺序 | 工单 | 线 | 交付物 | 状态 | 依赖 |
|---|---|---|---|---|---|
| 1 | C0 | 契约 | 阶段切换＋submit/continue/cancel/snapshot/subscribe/approval 最小 DTO 与语义 | **DTO 已落地（2026-09-07）**：`agent-platform-protocol/src/work.rs` 六条 run-scoped 路由＋事件通知 DTO＋金样序列测试；.NET 客户端同形镜像 | C0, P1 可开始 |
| 2 | B1 | 基础 | 监督身份、可靠台账、清理确认、宿主 proof 监督接线、watchdog 边界 | 主体已关闭（2026-09-07）：身份化台账/类型化对账门/清理回执/proof 接线/watchdog 组扫描，Windows＋WSL2 验证；扩展验收项随 P3 收口 | 可开始 |
| 3 | B2 | 基础 | metadata 发布不确定时的 writer 围栏 | 已关闭（2026-09-07）：`RecoveryRequired` 发布不确定＋compact 围栏＋注入测试；续审确认通过 | 可开始 |
| 4 | B3 | 基础 | Context 验证关联＋stdin/grant/输出接入边界 | 主体已关闭（2026-09-07）：同配方关联终结、stdin/grant 读取时计费、有界输出 sink；扩展范围（recipe 版本/覆盖身份关联、无期限 stdin 读取期限）见剩余表 | 可开始 |
| 5 | P1 | 平台 | 与 TUI 无关的原子工作提交与受理回执 | **已关闭（2026-09-07）**：`StartWork` 原子命令＋有界受理台账（同 id 同内容幂等、异内容拒绝）；TUI/无头共用 `agent_runtime::work`；F07 交叉投递 5 项验收测试全绿 | — |
| 6 | P2 | 平台 | 类型化快照、增量事件、审批与结果、重同步 | **主体已关闭（2026-09-07）**：runtime＋TUI 投影 revision 归属修复；`ToolFailureClass::ApprovalDenied` 类型化拒绝替代文字推断；`StatusSnapshot`＋watermark；`WorkControlRouter`（submit/continue/cancel/snapshot/subscribe/approval.respond＋会话授权）；事件重放窗口仍为 resync-only | — |
| 7 | P3 | 平台 | 正式 Rust 宿主＋Named Pipe/UDS 双向通信 | 已落地（2026-09-07）：`crates/agent-host` 编译＋命名管道 E2E 绿＋.NET 客户端互操作冒烟 exit 0；事件 wire 契约与 UDS 真机验证见 P3 节 | C0, P1, P2 |
| 8 | G1 | GUI | .NET 客户端库＋正式 Avalonia 外壳 | 已落地（2026-09-07）：`clients/dotnet/Agent.Client`＋`apps/Agent.Desktop`＋`global.json`(SDK 10.0.301)；C0 fixtures Rust/C# 双语一致测试绿；build/测试/启动冒烟通过 | C0 |
| 9 | G2 | GUI | 审批/取消/审阅/冷恢复完整链路 | 客户端与宿主链路已落地（2026-09-07）：重连不自动应答审批、断线/resync 横幅、宿主端审批经真实 gate 回执（E2E 绿）；差异按需读取与 GUI 端到端真实工具走查未做（等差异路由与真实 provider） | G1, P2, P3, B1, B2 |
| 10 | G3 | GUI | 长会话低开销工作台＋只读 Context 检查 | 客户端侧已落地（2026-09-07）：列表虚拟化＋有界保留（200 任务/400 行输出）、DeltaCoalescer、MetricsSession（未测写 NOT_RUN）、只读 Context 面板占位；首采样见 walkthroughs/2026-09-07-g3-desktop-metrics.md | G1, P2, B3 |
| 11 | E1 | 扩展 | 一个真实外围能力＋按需 Skill 最小闭环 | 已关闭（2026-09-07）：MCP 写/连接/读取消全贯通、compose 配置缝、真实 mock server 闭环集成测试、Skill 按需有界读取；Windows＋WSL2 验证 | P3, G1（闭环已先行落地） |
| 12 | R1 | 联合 | Rust＋.NET 来源绑定打包与正式使用收口 | 提案 | B1–B3, P3, G2, G3 |
| 13 | PACKAGE-01 | 条件 | 打包来源绑定 | 下次实际发布 | — |
| 14 | MCP-01 | 条件 | MCP 写/连接/读可取消 | 仅默认启用 MCP 时 | — |


## 上一阶段队列（已关闭，2026-09-06/07）

| 工单 | 交付物 | 状态 |
|---|---|---|
| ~~STORAGE-02~~ | 压缩已发布后失败则隔离旧 writer | 已关闭（2026-09-06）；2026-09-07 续审指出 helper 内 rename→目录同步残余 → B2 |
| ~~PROCESS-01~~ | 宿主验证硬崩溃监督 | 已关闭（2026-09-07）：Windows Job 围栏 + Unix 管道 EOF 看门狗 + 监督台账，全部在真 Linux（WSL2）验证；续审指出台账身份/确认残余 → B1 |
| ~~PROCESS-02~~ | reap 未确认退出不清 pid | 已关闭（2026-09-06） |
| ~~WORKSPACE-01~~ | 普通 open 不阻塞 FIFO | 已关闭（2026-09-06） |
| ~~WORKSPACE-02~~ | Windows 拒绝路径立即接管 HANDLE | 已关闭（2026-09-06） |
| ~~PROVIDER-01~~ | 错误 HTTP body 有界读取 | 已关闭（2026-09-06） |
| ~~PROVIDER-02~~ | Chat `length` 终止语义 | 已关闭（2026-09-06） |
| ~~PROVIDER-03~~ | Responses EOF 尾帧校验 | 已关闭（2026-09-06） |
| ~~CONTEXT-01~~ | 依赖候选 newest-first | 已关闭（2026-09-06） |
| ~~M16-02 剩余~~ | 待审阅 ≠ 持久完成 | 已关闭（2026-09-07） |

已关闭、跳过：STORAGE-01、DOC-01；EOF wait、消费 ACK、PromptRequired、resync。
关闭证据：STORAGE-02 `f9852ea`、PROCESS-01/02 `7c72df3`、WORKSPACE-01/02 `17c5ded`、PROVIDER-01/02/03 与 CONTEXT-01 `3e0128a`（定向测试计数见各提交说明）。
续审明确不再原样重报的旧问题：release 消费 ACK stamp、process.run 输出 EOF、Windows metadata 不再先 unlink、普通读取不再清错误、片段 supersession 覆盖判断——见 REPORT.md 第 4 节。

---

## C0 — 阶段契约（进行中：文档部分已落地）

**用户结果：** 不同入口使用同一操作、身份和快照语义；GUI 可直接进入长期实现。

**入口／拟新增路径：** `AGENTS.md`；`docs/CURRENT.md`、`docs/ROADMAP.md`、本文件（本次已改）；`crates/agent-platform-protocol/src/`（拟新增）。

**剩余步骤：**
1. 约定 submit/continue/cancel/snapshot/subscribe/approval response 的有限 DTO、身份、错误与大小上限；受理、应用、任务完成、清理确认分开。
2. 列清 supported/unsupported，不预定义全部未来 namespace。
3. 同一契约示例供 Rust/C# 使用；共享字段修改由单一负责人合入。

**检查：** `python scripts/doc_consistency.py`（本次已跑）；DTO 落地后加对示例的双语言含义一致测试。

**做到这里停止：** 不新建文档治理框架；不把"全部平台协议设计完"作为后续工单的开始条件。

## B1 — 监督身份、台账、宿主验证接线与有界清理

**用户结果：** 不凭旧 PID 误杀其它进程；未确认清理不丢监督记录；真实宿主 proof 路径应用与普通工具相同的监督策略。

**入口：** `crates/tool-runtime/src/supervision.rs`、`crates/tool-runtime/src/proof_runner.rs`、`crates/tool-runtime/src/tools/process.rs`、`crates/agent-process/src/watchdog.rs`、`crates/agent-process/src/lifecycle.rs`、`crates/agent-compose/src/lib.rs`。

**步骤：**
1. 复用 `lifecycle.rs` 的 `ProcessIdentity`、`inspect_process` 与 `terminate_matching_process_tree`；区分退出、身份不符、清理已确认与无法确认，观测错误不得成为退出证明；旧的无身份记录默认不得 kill（对齐 F01/F02）。
2. 台账记录/读取/结束返回 `Result`；限制大小与行数、串行化修改、必要耐久；读失败与损坏不得混同于"无未决进程"。
3. 以明确 finished/reaped 回执释放记录；`ChildLease::Drop` 只做保守清理，不伪造确认。
4. 监督配置由宿主统一注入普通 dispatcher 与 `RecipeProofRunner`（修 F03 的 `ProcessRunTool::new` 默认关闭）；re-exec marker 不隐式依赖传播到任意客户端可执行文件。
5. 明确组长/成员/正常解除语义：watchdog 在 exec 前加入孩子的独立进程组，持续保住组身份；Drop、终止 helper 与清理后的 wait 均有界，无法确认则保留责任。Windows session 沿用既有 Job 围栏，未确认退出不得生成耐久完成回执。

**已执行的定向检查（2026-09-07，当前工作树）：** Windows 进程库 38、工具库 246（另有 1 项原有 ignored；新增未知退出用例后 session 单独复验 14）、process_journal 8、依赖边界 3、compose 硬退出/监督门禁/恢复 6 项通过；上下文与 GC 23、Core 审批 1、Actor 工作入口 18 项定向复核通过。WSL Linux 的 watchdog 单元 6 / 真实子进程 4、监督台账 17、进程日志 7、session 14、process.run 19、exact-proof 宿主硬退出 1 项通过。相关 crate 的 all-targets 检查通过。完整命令与实现证据见 [核心边界与宿主清理报告](reviews/2026-09-07-worktree-review/CORE_BOUNDARIES_AND_HOST_CLEANUP.md)。

**剩余验收：** 正式 P3 宿主的同等接线与硬退出路径；spawn 到 Job/watchdog 就绪的窗口、OS 拒绝容器/监督启用、子进程主动脱离进程组，以及遗留无容器孤儿的冷恢复。当前 Windows 根句柄与 Unix watchdog 组身份保证各有明确范围，不能外推为所有后代遍历和冷恢复 PID 竞态均已消除；B1 尚未全部验收完成。

**做到这里停止：** 不新建通用 Scheduler；受支持的执行/恢复声明不得先于本工单验收。

## B2 — metadata 已发布但目录同步失败的围栏（已关闭 2026-09-07）

**用户结果：** 一次压缩返回不确定错误后，本进程不会继续健康地向旧代写入。

**已落地：** `persist_authority_metadata` 在 rename 发布之后才可能失败的目录同步改为 `AgentError::RecoveryRequired`（"可能已部分落地"语义）；`compact_locked` 对该变体设置 `writer.failed` 围栏——显式 `compact_authority_journal` 与追加触发压缩同走此路径，不能留下健康的旧代 writer。故障注入为 thread-local 的 `SYNC_DIRECTORY_FAULT` 切点（唯一发布后失败点），不触碰真实目录同步屏障。

**检查：** `cargo test -p agent-storage` 22/22（Windows，并行 ×3 稳定；WSL2 同过），含新增 `compaction_publish_uncertain_failure_fences_the_writer`：发布不确定错误 → 后续 append 被拒 → 重开承认已发布 g2 并继续追加（seq 3）。`cargo test -p agent-core` 全绿。续审确认"已有实质修复……本轮通过"（reviews/2026-09-07-worktree-review/REVIEW.md）。

**做到这里停止：** 只修发布语义；不删目录同步换测试绿；不新增数据库/Chronicle/第二套日志协议。

## B3 — Context 验证关联与运行边界

**用户结果：** 错误不被同实体的无关验证终结；外部输入/输出不在进入 Runtime 前后绕过限额。

**已落地（2026-09-07，F08＋F09 主体）：**
1. **验证关联（F08）：** `ContextItem`/`ExternalizedContext` 新增 `verify_recipe`（serde default，兼容旧检查点）；verify.run 失败把 `metadata.recipe_id` 盖到错误上，成功只有携带**同一 recipe_id** 才排队终结；`queue_error_verifications` 不再按实体重叠匹配，无关联的成功一律保持 live。
2. **读入计费（F09）：** `resolve_prompt` 的 stdin 路径 `take(USER_INPUT_REPLAY_MAX_BYTES+1)` 有界读入（读入阶段即拒，不做全量分配）；`load_grant_file` 改为 take 上限读取，stat 仅为 regular-file 检查。
3. **有界输出 sink（F09）：** `run_headless` 改为移交 writer 所有权，专用写线程＋64 行有界队列；事件循环不再被慢 stdout/文件阻塞（超时/取消保持有效）；慢消费者/断线以类型化失败结束（截断流绝不报 exit 0），关闭等待有界（10s）。

**检查（已执行，2026-09-07）：** `cargo test -p context-simple` 288 全绿（含新增 `unrelated_successes_never_finalize_an_error`：不同配方成功、普通工具成功均不终结，同配方成功终结）；lifecycle/residency fixture 更新为同配方契约。`cargo test -p agent-tui` cli 16 项全绿（含 `stdin_prompt_is_charged_at_read_time`、`grant_file_over_the_cap_is_refused`、`headless_output_disconnect_ends_the_run_with_a_typed_failure`）。

**剩余范围（续审指明，随 B3 后续/P2 收口）：** 可信 recipe 的版本/覆盖身份及任务、故障级关联保存与核对；无期限且未达 cap 的 stdin 读取期限；终端 IO helper 不当成正式 GUI 客户端实现。

**做到这里停止：** 不重调 GC 阈值，不同时引入 BM25/向量/缓存算法。

## P1 — 与 TUI 无关的原子工作提交与受理回执

**用户结果：** TUI、GUI、SDK 不能把指令误投给另一客户端刚切换的任务（修 F07）。

**入口：** `crates/agent-tui/src/work.rs`（现共享工作流）；`crates/agent-runtime/src/command.rs`、`crates/agent-runtime/src/actor/commands.rs`；`crates/agent-compose/src/lib.rs`。

**步骤：** 共享工作入口移入公共应用层；实现原子 start_work 或显式 task/expected revision 提交，复用既有 TaskManager prepare/commit；返回稳定受理身份，同 client request id＋相同内容有界去重、异内容拒绝；无可靠回执返回 unknown/要求查询，不自动换 ID 重放副作用。

**检查（建议，未执行）：** `cargo test -p agent-runtime`；`cargo test -p agent-tui`。验收：两客户端交错 SetFocus/Submit 不跨任务投递；重复请求不偷偷再执行；单客户端任务身份不变。

**做到这里停止：** 不远程导出整个 `RuntimeCommand`；不公开恢复半事务或 `CorePort`；不建第二个 TaskManager。

## P2 — 类型化快照、增量事件、审批与结果

**用户结果：** 新客户端接入、重连、慢消费后显示正确状态，不解析终端文字（修 F06）。

**入口：** `crates/agent-runtime/src/status.rs`（`anchor_revision` 跨任务 `max`）；`crates/agent-contracts/src/event.rs`；`crates/agent-core/src/approval.rs`；`crates/agent-tui/src/cli.rs`（文字推断审批）；`crates/agent-runtime/src/platform/`。

**步骤：** 修 revision 归属；移除从任意 ToolOutput 文本推断审批拒绝；一致快照＋watermark＋其后事件、有限重放窗口与 `resync_required`；实时文字流偏移与耐久事件序列分开；审批响应绑定 request/run/operation 与会话，重连可查 pending；慢消费者有界队列，不静默抹掉审批/终态、不阻塞 Actor。

**检查（建议，未执行）：** `cargo test -p agent-runtime`；`cargo test -p agent-tui`。验收：A revision9→B revision1 展示与 API 均为 B=1；工具正文含拒绝短语不改变真实审批状态；缺口被明确报告。

**做到这里停止：** 只建可重建投影；不建 Chronicle 数据库；投影不反向提交 effect。

## P3 — 正式 Rust 宿主与本地双向 RPC（已落地 2026-09-07）

**已落地：** `crates/agent-host`（成员已入 workspace）。薄宿主二进制镜像 TUI 组合根（同一 kernel/tools/approval/context 选择），`--pipe`/`--socket`/`--read-only`/`--restore-latest`，`host.lock` workdir 单实例（进程身份核对陈旧锁接管）。Windows 命名管道：`PIPE_REJECT_REMOTE_CLIENTS`＋仅当前用户 DACL＋逐连接客户端令牌 SID 校验；Linux UDS：SO_PEERCRED；无法验证的对端在首帧前丢弃（fail closed）。每连接服务端安装 WorkControlGrant（operator/read-only）＋独立绑定 authorizer，wire 字符串不自报授权。帧＝4-byte LE＋JSON、1 MiB 帽（与 .NET 客户端一致）＋协议 crate DOM 预算；畸形帧关连接；未知路由回 `route.unsupported`。事件通知 wire 契约随 P2 类型化事件落地，客户端先以快照重建（诚实不丢）。

**已验证：** `cargo test -p agent-host` E2E（命名管道）：提交受理/幂等重试 AlreadyAccepted/同 id 异 goal 结构化拒绝 `work.rejected`/快照焦点绑定/真实 gate.authorize 注入审批→快照可见→服务端绑定 id 应答 Delivered→挂起决策解析为 Allow/cancel 诚实 ack/未知路由拒绝。互操作冒烟：真实 .NET Agent.Client 连真实宿主默认管道——连接、快照、提交、焦点、取消 `NoActiveTurn`、干净退出（exit 0）。发现并修复 C# 转换器 HashSet 预置 "status" 导致合法字段被拒的 bug。

**未验证/限制：** UDS 路径仅代码＋编译，真 Linux 运行待 CI（Windows 开发机无 UDS）；事件 wire 契约未定义（订阅回 watermark，通知接收方为后续）；UDS 读期限有（120s），命名管道阻塞读依赖本地可信对端＋有界帧，未做读写期限。

**用户结果：** 原生 GUI 与其它入口连接同一工作区宿主，不各自打开一份可写运行状态。

**入口／拟新增路径：** `crates/agent-runtime/src/platform/session.rs`；`crates/agent-process/src/session.rs`；`crates/agent-platform-protocol/src/`；`crates/agent-compose/src/lib.rs`；`proposed: crates/agent-host/`。

**步骤：** 薄宿主可执行文件（生命周期、workdir 单实例、watchdog marker 在 Rust 宿主负责）；Windows Named Pipe／Linux UDS 同一有界 framing，OS 后端隔离；连接 ACL/peer 身份在服务端落实，客户端不得自报提权；帧与 decoded DOM 上限、并发/队列/读写期限；接收循环不等整个任务结束才读 cancel；关闭窗口、断线、宿主退出、task cancel 区分，提供显式后台继续或停止策略。

**检查（建议，未执行）：** `cargo test -p agent-platform-protocol`；`cargo test -p agent-process`；`cargo test -p agent-runtime`。验收：半帧/粘帧/超大帧/无权会话正确处理；第二客户端附着既有宿主；B1/B2 未完成时可开发只读连接，但不开放相关可靠执行/恢复承诺。

**做到这里停止：** 不做系统级常驻服务、不默认公网监听、不复制 codec 和 authority；不同时做 HTTP/TCP/gRPC。

## G1 — .NET 客户端库与正式 Avalonia 外壳（已落地 2026-09-07）

**已落地：** `clients/dotnet/Agent.Client`（net10.0 类库，零 Avalonia 依赖、零 P/Invoke）：run-scoped DTO 逐字节镜像 Rust `work.rs` wire 形状（snake_case、deny-unknown、serde 变体名原样 Only "Allow"/"Deny"），有界帧（4-byte LE、1 MiB），request-id 关联，本地等待取消与显式 `work.cancel` 命令分离，Named Pipe/UDS 传输，ResumableSession 重连策略。`clients/dotnet/Agent.Client.Tests`（24 项）与 `crates/agent-platform-protocol/tests/work_fixtures.rs`（9 项）读同一批 `tests/fixtures/work/*.json`：解码＋验证＋重编码逐字节一致（C0 双语一致性验收）。`apps/Agent.Desktop`（Avalonia 11.3.20）：任务列表/目标提交/继续/取消/审批卡/快照驱动状态条，异步 IO 不占 UI 线程，PerMonitorV2 manifest，无 WebView。`global.json` 锁 SDK 10.0.301。布局夹具模式明确标注"非执行器"。

**已验证：** `dotnet build` 两项目 0 警告 0 错误；`dotnet test` 24/24；`cargo test -p agent-platform-protocol` 35＋9 绿；GUI 6 秒真实启动冒烟；互操作冒烟（见 P3）。

**未验证/限制：** 事件流消费等待 P2 wire 契约（客户端订阅已实现，通知流为空）；中文输入法/DPI/大文本未做专项实测；plan/Context 面板等平台字段。

**用户结果：** 第一版就是正式原生客户端；通信层可被其它 .NET 应用复用。

**拟新增路径：** `clients/dotnet/Agent.Client/`、`apps/Agent.Desktop/`、`global.json`（均拟新增，锁定实施时确认的 .NET 10 SDK 版本）。

**步骤：** class library 与 Avalonia app；Agent.Client 不依赖 Avalonia、不 P/Invoke Runtime；DTO 用 C0 共同规范与跨语言样例；请求关联、取消等待与显式取消命令区分、事件流与帧上限；先用同 DTO 有限 fixture 驱动布局，P1/P2 可用后连真实宿主（fixture 不是模拟执行器）；正式窗口含任务选择、输入/输出、计划、状态，异步读写不占 UI 线程。

**检查（建议，未执行）：** `dotnet build clients/dotnet/Agent.Client/Agent.Client.csproj`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj`。验收：client library 可在无 GUI 测试程序使用；正式 GUI 连真实 Runtime 取快照并提交/继续；主界面不依赖 WebView。

**做到这里停止：** 不先做 IDE、插件 UI SDK 或全量自绘控件库；不用第二个"验证 GUI"替代。

## G2 — 审批、取消、审阅与恢复完整操作链（客户端＋宿主链路已落地 2026-09-07）

**已落地：** 桌面审批卡走平台绑定 id 并等真实回执（宿主 E2E 证明 Delivered/NoLongerPending 语义）；ResumableSession 断线后重连强制快照重建，`RespondApprovalAsync` 不自动重试（丢失的 Allow 可能已送达也可能没有，重发即伪造同意），重连横幅显式声明"挂起审批不会自动通过"；取消走 `work.cancel` 并如实区分 Cancelled/NoActiveTurn；冷恢复在宿主侧走完整 `RuntimeInstance::restore` 事务（`--restore-latest`），窗口重开即快照同步。

**已验证：** 宿主命名管道 E2E（审批全链路）＋ .NET 互操作冒烟；`dotnet test` 24/24 含"断线不自动应答审批"专项。

**未验证/限制：** GUI→真实工具→差异审阅端到端待差异/工件读取路由与真实 provider（不可用写 NOT_RUN）；预算让出/证据完成/操作员接受/恢复受阻的细分展示待 P2 类型化任务字段（快照目前只有 active/suspended/completed）；含用户预存修改的冷恢复 GUI 走查未跑。

**用户结果：** 用户能执行真实开发任务，并知道改动、检查、未验证状态与可恢复点。

**拟新增路径：** `apps/Agent.Desktop/`、`clients/dotnet/Agent.Client/`；`crates/agent-runtime/src/instance.rs`。

**步骤：** 审批卡显示平台返回的绑定操作/范围，点击等真实回执；区分运行/预算让出/待审阅/证据完成/操作员接受/恢复受阻；差异与工件按需授权读取，保留用户已有修改；重开走快照/事件同步，冷恢复走完整 `RuntimeInstance` 事务；中文输入法、复制、键盘、DPI、大文本正常。

**检查（建议，未执行）：** `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj`；`cargo test -p agent-compose`。验收：GUI→任务→真实工具/审批→取消/续跑→差异审阅端到端；丢连接不自动通过 pending 审批；含用户预存修改的冷恢复不重复副作用。

**做到这里停止：** 不加第二执行器；不做全量通用代码编辑器；不因 GUI 增加绕过 Core 的文件写入口。

## G3 — 低资源使用的正式工作台与只读 Context 检查（客户端侧已落地 2026-09-07）

**已落地：** 列表虚拟化（Avalonia ListBox 默认虚拟化栈）＋底层保留上限（任务 200 条、输出尾部 400 行、通知队列 1024）；DeltaCoalescer 小窗口合并流式文字（不丢字符，事件契约落地后接线）；只读 Context 面板占位（平台路由前不显示推断内容）；MetricsSession 有界测量记录器（全进程树工作集采样，未测指标写 null=NOT_RUN）。首采样：[walkthroughs/2026-09-07-g3-desktop-metrics.md](walkthroughs/2026-09-07-g3-desktop-metrics.md)（空闲工作集 222–232 MB，单环境调试构建，不构成性能结论；长会话斜率/输出 CPU/大 diff 均 NOT_RUN）。

**用户结果：** 长会话、大 diff 和上下文查看不导致全历史反复传输、解析和渲染。

**拟新增路径：** `apps/Agent.Desktop/`；`crates/agent-runtime/src/platform/`。

**步骤：** 列表虚拟化同时限制底层保留数据；流式文字按小窗口合并；大正文只传 locator/元数据/分页片段，视图关闭释放缓存；Context 面板只读显示来源、表示类型、实际曝光、片段范围、恢复状态（不显示不存在的模型内部注意力）；记录全进程树空闲内存、长会话斜率、输出时 CPU/分配、大 diff 响应，无测量写 `NOT_RUN`；AOT/裁剪单独做兼容性核查，不作首窗前置。

**检查（建议，未执行）：** `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release`。验收：固定大型会话/差异 retained data 有界；工具/审批/终态事件不为流畅显示静默丢失。

**做到这里停止：** 不建遥测平台；不写零分配通用 UI；不以研究性 GC 算法胜出当 GUI 完成标准。

## E1 — 一个真实外部能力与按需 Skill 的最小闭环（已关闭 2026-09-07）

**用户结果：** 正式客户端能使用一个实际外围能力；Skill 不常驻所有上下文。

**已落地（2026-09-07）：**
1. **真实外部能力选型：MCP stdio server**（复用既有 `McpCapabilityAdapter`/`McpClient` 沙箱栈：scrubbed env、私有 cwd、landlock/integrity、supervisor 树杀）。
2. **MCP 取消贯通（MCP-01 范围，不以"默认未开启"豁免）：** `request_with_cancel` 的写阶段改为与 cancel select 组合（取消即时生效，部分帧按既有 poison+kill-then-reap 收尾）；新增 `initialize_with_cancel`/`list_tools_with_cancel`/`connect_stdio_with_cancel`——连接（spawn+握手）阶段可取消，取消不被重启熔断记为服务器故障。测试：写阶段取消（对端停读时 50ms 内 Cancelled，不等 30s deadline）、连接阶段取消（静默服务器 150ms 取消即收尾并树杀）。
3. **compose 配置缝：** `ComposeConfig` 新增 `mcp_servers`（配置→发现→注册→显式 enable；发现失败组合失败 fail-closed）与 `plugins`（注入 dispatcher）；风险从声明权限推导，从不信服务器自述。
4. **Skill 按需读取：** `PluginRegistry::install_from_root`/`skill_read`——双激活门（包 Active＋Skill Active）、引用路径围栏（准入拒绝逃逸 + 读取时二次防御）、64 KiB 有界读（超限拒绝不截断）；`capability.manage` 新增 `read_skill` op，正文作为普通工具输出带 provenance/version 返回，从不自动注入、从不成为 system 权威。
5. **闭环集成测试**（`tests/e1_mcp_loop.rs`，真实 mock server 进程）：配置→发现→注册→search（未加载工具不在模型表面）→load（进入表面）→invoke（有界回显＋心跳）→shutdown（心跳停跳＝进程树确死，清理可观测而非假设）。

**检查（已执行，2026-09-07）：** `cargo test -p agent-capability-process` 24 lib＋26 capability_host＋1 闭环（Windows 与 WSL2 双侧全绿）；`cargo test -p agent-runtime --lib plugin:: capability::tests::read_skill` 9 项全绿（含 read_skill 双门、逃逸拒绝、超限拒绝、无目录拒绝）。验收对照：新增能力零 RuntimeActor 特判（注册即被统一目录/加载/调用面消化）；未加载能力 schema 不注入（search 轮断言 mock.echo 不在表面）；停机清理可观测（心跳停跳断言）。

**做到这里停止：** 不建插件市场；不要求子 Agent/DAG/递归才完成本切片。

## R1 — Rust＋.NET 来源绑定发布及正式使用收口

**用户结果：** 安装包里的客户端/宿主确属本次构建，版本匹配，真实使用与验证范围明确。

**入口：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`；拟新增桌面打包；`docs/CURRENT.md`、`docs/COMPATIBILITY.md`。

**步骤：** 修自定义 target 未传 Cargo、陈旧 dist 混入、PowerShell 退出码（即 PACKAGE-01 范围，届时一并关闭）；记录 Rust 源码 SHA、.NET/协议版本、构建配置与 checksum；现有跨平台 CI 加最小 C# 构建/协议相容项；正式 GUI 完成小 bug、多文件功能、断线/冷恢复续跑（真实 provider 不可用写 `NOT_RUN`）；实现/定向测试/默认启用/真实使用四类状态分别更新。

**检查（建议，未执行）：** `cargo fmt --check`；`cargo clippy --workspace --all-targets -- -D warnings`；`cargo test --workspace`；`dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release`。这些是集成/发布检查，不是每次改文档都执行。

**做到这里停止：** 不要求所有未来扩展实现才发布当前正式 GUI；失败构建不输出成功包。

---


---

## STORAGE-02：压缩发布后的 writer 围栏（已关闭 2026-09-06）

**实现：** `compact_locked` 的全部可失败步骤（新 WAL seek、next_seq 溢出检查、恢复态重建）移到 `persist_authority_metadata` 发布点之前；发布后只剩纯内存 writer 交换与尽力删除旧 WAL，结构上不可能再把 writer 留在旧代。新增定向测试：新 WAL 创建失败（发布前）保持旧代可追加且元数据未动；发布后/删除旧 WAL 前的崩溃窗口重开按已发布代际续写。agent-storage 21/21。

**用户结果：** 压缩元数据已经写到磁盘之后，若 seek、切 writer 或显式 compact 调用失败，本进程不能继续往旧代追加；再次打开与对账结果一致。

**入口：** `crates/agent-storage/src/lib.rs`（`compact_locked`、`persist_authority_metadata`）；`crates/agent-core/src/operation.rs`（`compact_authority_journal` 目前只转发错误）。普通 `append_transition` 已有围栏，不要拆掉。

**步骤：** 读 compact 成功发布 metadata 之后的失败返回点；失败后隔离旧 writer（或等价的本进程写入围栏）；打开路径与 STORAGE-01 一致：缺 metadata 且有代际则 `RecoveryRequired`，不选最大 `.gN`。

**检查：** `cargo test -p agent-storage`；若改了 Core 转发路径，加一条显式 compact 失败后不可再追加的定向测试。不跑全仓。不注入真实 Windows 进程崩溃。

**做到这里就停。** 不建 Chronicle；不把 STORAGE-01 的 Windows 崩溃注入当成本工单。

---

## PROCESS-01：宿主验证硬崩溃后的监督

**用户结果：** 开启宿主验证时，宿主进程被 SIGKILL/abort 后，不留下可继续改工作区的无监督子进程。

**已落地（2026-09-07）：** ① Windows KILL_ON_JOB_CLOSE 围栏（`7c72df3`）；② Unix 管道 EOF 看门狗（`a954314`：re-enter 宿主可执行文件 + socketpair，宿主死亡 → EOF → 确认组长仍活 → `kill(-pgid)`；正常 reap 后 drop 写端解除）；③ 监督台账（`995a457`：execute_invocation 经 ChildLease 记录每个子进程，正常路径释放、崩溃路径留行，compose 启动时对账清理后才复用工作区；pid 精确匹配、死条目清除防复用误杀）。**已验证（2026-09-07）：** 全部 cfg(unix) 测试在真实 Linux（WSL2 Ubuntu，真内核）执行通过——agent-process 40/40（含看门狗 6 项：EOF 杀活组、reaped leader 不杀、pid-0 拒绝、Drop 收割看门狗、管道接线）、agent-workspace 132 项（WORKSPACE-01 FIFO、WORKSPACE-02 句柄路径）、tool-runtime 229 项（台账对账杀 abandoned 子进程）。CI 重跑转为回归确认。

**入口：** `crates/tool-runtime/src/proof_runner.rs`、`crates/tool-runtime/src/tools/process.rs`。Linux 探针已证 OS 机制（父死子存），不是 Agent 集成测试。

**检查：** 定向 `tool-runtime` / 现有 crash fixture；监督身份与权限身份分开。不否定已落地的取消桥接（F3-6b）。

**不要：** 为这件事新建 Scheduler。未跑 Agent crash-resume 不写已闭环。

---

## PROCESS-02：未确认退出不清 pid（已关闭 2026-09-06）

**实现：** `reap` 返回类型化 `ProcessReapOutcome`，仅在确认退出后清 pid，未确认终态由 Drop 保留击杀责任；`kill_tree` 直接子进程 fallback 改为无锁 direct pid kill（原 `try_lock` 在 reap 持锁时必失败）。回归：持锁期间非 group-leader 子进程经 fallback 终止（`7c72df3`；agent-process 30）。

**用户结果：** `reap` 在 wait 失败或两次有界等待都超时时，不能报告清理成功并丢掉监督身份。

**入口：** `crates/agent-process/src/supervisor.rs`（`ProcessSupervisor::reap`、`kill_tree`）。

**检查：** 注入 wait 错误 / 第二次 wait 超时的定向测试。Unix group leader 不能当作本工单已证明。

**不要：** 用“再 kill 一次”冒充确认退出。

---

## WORKSPACE-01：普通 confined open 不阻塞 FIFO（已关闭 2026-09-06）

**实现：** 普通 open 带 `O_NONBLOCK`（普通文件/目录 I/O 不受影响），同句柄 stat 拒绝非普通文件/目录，staged 目标要求普通文件；`project_markers` 改为仅元数据探测（`fstatat AT_SYMLINK_NOFOLLOW`），根扫描不再打开任何条目。回归：无写端 FIFO 位于 `Cargo.toml` 不再卡住扫描（watchdog 测试；`17c5ded`，agent-workspace 98+5+3）。

**用户结果：** 项目标记探测遇到无写端 FIFO 时有界失败，不在同步 `open` 上卡死。

**入口：** `crates/agent-workspace/src/confined.rs`、`runtime_facts.rs`（`project_markers`）。recovery 路径已有 `O_NONBLOCK`，复用它。

**检查：** Unix FIFO 名为 `Cargo.toml` 的定向测试；普通文件、`.git` 目录、symlink/reparse 不被误伤。正文只进入支持的文件类型。

**不要：** 把所有标记都当普通文件打开。

---

## WORKSPACE-02：Windows 拒绝路径立刻接管 HANDLE（已关闭 2026-09-06）

**实现：** 六处 raw HANDLE 调用点（`open_root_handle`、`open_child_dir`、`open_existing` 两臂、`open_staged_for_cleanup`、`open_or_create_regular_file`）全部先 `from_raw_handle` 接管再 `check_not_reparse`，对齐 recovery helper 模式。既有 reparse 拒绝测试覆盖行为；句柄计数故障注入未做（`17c5ded`）。

**用户结果：** `check_not_reparse` 失败时句柄仍被拥有并关闭，拒绝仍然发生。

**入口：** `crates/agent-workspace/src/confined.rs` 多处 raw HANDLE。复用已有 recovery helper。

**检查：** 现有 Windows confined 拒绝测试仍通过。未做句柄计数故障注入则写明。

**不要：** 把“拒绝发生”说成路径逃逸已存在。

---

## PROVIDER-01：错误响应有界读取

**用户结果：** 非 2xx 的巨大或持续 body 在读入阶段被限额，不先 `text().await` 再截短。

**入口：** `crates/provider-openai/src/lib.rs`（`complete_chat_stream` / `complete_responses_stream`）。

**检查：** loopback 夹具：大 body、持续 body、多字节 UTF-8；取消与超时仍正确。

**不要：** 改模型协议权威；无压力测试不写已抗资源耗尽。

---

## PROVIDER-02：Chat length 终止

**用户结果：** Chat `finish_reason=length` 与 Responses 输出上限一样，保留不完整终止，不丢成正常完成。已暴露的输出不透明重放。

**入口：** `crates/provider-openai/src/sse.rs`、`lib.rs`、`responses.rs`。

**检查：** Chat length+DONE 与 Responses `max_output_tokens` incomplete 成对夹具。

**不要：** 无条件重试。

---

## PROVIDER-03：Responses EOF 尾帧校验

**用户结果：** 最后一帧无论是否空行结束，event 名与 JSON type 矛盾时都拒绝。

**入口：** `complete_responses_stream` 的 `framer.finish()` 路径；与正常 SSE 共用 handler。

**检查：** 同一矛盾帧有/无尾部空行的夹具。

---

## CONTEXT-01：依赖候选 newest-first

**用户结果：** 同一实体超过 64 条 live 条目时，候选仍按 newest-first，而不是先截创建序旧前缀再排序。

**入口：** `crates/context-simple/src/index/dependency.rs`（`push_linked`）、`indexes.rs`（`update_entities` / `swap_remove`）。

**检查：** 64/65/128 条；dead 前缀不耗尽配额。扫描工作量与候选配额分开。

**不要：** 重写选择器或调 GC 阈值。关联边不是强制正文 Continuation。

---

## PACKAGE-01：打包来源绑定

**用户结果：** 一次发布复制的二进制就是这次构建写出的那份；旧 `dist/<version>` 和自定义 target 不能拼出“成功”的旧包。

**入口：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`。Bash 桩测已证明旧产物可被打包。

**何时做：** 下次实际发布或主动打包装时，不要提前改脚本充数。PowerShell 原生退出码一并核对。

**不要：** 宣称当前已发布 ZIP 已错；不为它新建评测框架。

---

## MCP-01：MCP 取消覆盖写阶段

**用户结果：** 对端停读时取消能打断写请求，不等完整 `request_timeout`；半帧 session 被毒化；清理状态有 reap 依据。

**入口：** `crates/agent-capability-process/src/mcp.rs`（`request_with_cancel`）。

**何时做：** 仅当默认产品声明启用 MCP 写路径。当前未启用则跳过，不算阻塞队列里的代码项。

**不要：** 第二调度器。

---

# M16：可持续交付的本地单 Agent

审查剩余关闭后再推进这里的产品剩余。阶段目标不变：用户给仓库级任务，Agent 能短计划、查读修改、补充、有限执行、停止、冷恢复、可审阅结果，以及同一 Runtime 的非交互入口。

提案原文不进默认必读：[reviews/2026-09-06-m16-proposal/TRIAGE.md](reviews/2026-09-06-m16-proposal/TRIAGE.md)。

| 工单 | 交付物 | 本分支状态 | 旧映射 |
|---|---|---|---|
| M16-00 | 切换活动路线 | 已切换 | D0 |
| M16-01 | 继续、忙时补充、启动预检 | 已关闭（2026-09-07）：走查转为自动化 TUI E2E | F1 |
| M16-02 | `/work` `/plan` 与完成语义 | 已落地（2026-09-07：待审阅/持久完成显式区分） | F2 |
| M16-03 | 有限模型轮与可确认取消 | 主体已落地；PROCESS/PROVIDER 见上列 | F3 |
| M16-04 | 可信冷恢复 | 已关闭（2026-09-07）：产品配置冷恢复走查落地（`e6795ed`） | 恢复路径 |
| M16-05 | `/review` 与状态区分 | 已关闭（2026-09-07）：双工作区走查转为无头 + TUI E2E | F4 |
| M16-06 | 上下文/搜索正确性 | 主体已落地；CONTEXT/WORKSPACE 见上列 | F5 |
| M16-07 | 单进程非交互入口 | N1/N2 已落地 | 原 F6 之后项 |
| M16-08 | 试用包与三类走查收口 | 无头 live 已有；TUI 交互走查已自动化（E2E）；PACKAGE-01 见上列 | F6 + 发布 |

---

## M16-00：切换活动路线

**用户结果：** 开发者进入仓库后看到一条当前队列，不会回到 M15 候选选择或 Chronicle 建设令。

**已做（2026-09-06）：** CURRENT / ROADMAP / 本文件改为 M16；随后把深入续审仍开放项提到本队列前面。提案原文入库审查目录。文档检查：`python scripts/doc_consistency.py`。

---

## M16-01：接通交互控制与可信启动

**用户结果：** 能继续原任务；忙时补充能看到受理/排队/拒绝；纯配置错误不先建 workspace 状态。

**已落地：** `/continue` → `continue_active_task()`；忙时单槽队列；命令错误走 notice；未知 flag 拒绝；release 消费盖章与 PromptRequired 判重（`c6fbbab`）。参数解析在打开 workspace 之前。

**已全部落地（2026-09-07，含预检收口 `fa2b6d1`）。** 原手工走查转为会话循环自动化 E2E（`tui_e2e_*`）：恢复后 /continue、忙时第二条输入可见排队并在下一 turn 应用。模型配置校验已前移到 workspace 创建之前（doctor 保持无需 key、先于其退出），参数解析、冲突检查、grant 校验、模型校验现在全部是纯预检；守卫是真实二进制测试 `real_binary_startup.rs`（坏 key 报错且不留 `.focus-agent`；AGENT_DEMO 真实二进制 headless 跑通，session_end 带待审阅语义）。不抽 CompositionPlan。

**不要：** 用 `user_message("继续")` 代替 continue；无界输入队列；后台服务。

---

## M16-02：任务工作模式、短计划与完成语义（已关闭 2026-09-07）

**已全部落地。** 入口（`/work`、`/plan`）与完成语义展示均已关闭；细节见下。

**用户结果：** 一个开发目标有短清单；用户能区分「已产出待审阅」「本段结束」「证据完成」「操作员接受」。

**已落地：** `/work`（focus + 空需求集上 `task.manage` PreferSurface + 一次 user-message）；`/plan` 读只读 `TaskPlanView`；清单随检查点往返。

**本切片剩余：已全部落地（2026-09-07）。**

1. 默认 `OperatorClosureOnly` 在 TUI / 无头结果里明确显示为待操作员审阅关闭。普通 final 结束 turn，不显示为持久 `TaskCompleted`。**已落地：StatusProjection 任务行带 [awaiting operator review] / [durably completed (operator accepted)]；TUI turn 结束时对活动任务显式提示；无头 session_end 增 `task_state` 字段（operator_accepted / awaiting_operator_review / none）。**
2. `/done` 走既有操作员接受路径，不伪造验证 PASS。**既有路径未动。**
3. `EvidenceRequired` 仅在宿主已声明准则与 coverage 时使用；普通 `cargo test` / `npm test` 保持 `TaskScoped`。**既有语义未动。**
4. `next_action` 仍是建议；不增加“必须为空才能完成”。计划 `[x]` 不是证据。**未变。**

没有可信验收域时，把结果交给用户审阅就是完整产品行为，不要为此新建通用验证器。

**检查：** 定向 `agent-tui` 状态投影 / 无头 `session_end`；复用现有 OperatorClosureOnly / task.manage 回归。不跑全仓。

---

## M16-03：有限执行段与可确认的进程取消

**用户结果：** 较长任务在有限模型轮后安全让出并显式续跑；慢命令/验证不会让控制入口失去响应。

**已落地：** `--max-rounds` 按模型轮解析；RoundBudget 让出并落安全点；EOF 后仍守超时/取消；验证取消 token 桥接 Actor。

**代码缺口改走前列 PROCESS/PROVIDER。** `--defer-proof` 改产品默认仍等真实慢验证走查。

**不要：** UI 自动无限续跑绕过用户上限；独立 Scheduler / worker pool。

---

## M16-04：可信冷恢复与最小历史入口（已关闭 2026-09-07）

**用户结果：** 关掉进程后能选经过验证的检查点，恢复同一任务并继续；缺失 metadata 不能当成新工程。

**已全部落地：** STORAGE-01/02；`RuntimeInstance::restore` 完整 prepare/finalize 事务；`--restore=latest`；产品配置「保存 → 结束组合 → 新组合 restore → continue」走查落地为 `agent-compose/tests/m16_restore.rs`（只读基策略 + standing grants 非 permissive 审批、capability-aware、持久预留 journal 跨重启、启动对账台账；两段各写一次、互不冒认）。`crash_resume.rs` 保留真实子进程崩溃矩阵。

**不要：** Chronicle / RunCatalog 数据库。

---

## M16-05：结果审阅与统一状态展示

**用户结果：** 能看到改了什么、哪些检查真正执行、哪些未验证；`/review` 不调模型、不绕过 Core 跑工具。

**已落地：** 事件派生结果卡；不归属用户原有修改；resync 最新优先/当前 run/水位/有界读；run_summary 按 entries+omitted。

**已全部落地（2026-09-07）。** 双工作区走查转为自动化 E2E：无头变体（`e2e_bug_fix_preserves_user_modifications`）+ TUI 交互变体（`tui_e2e_review_attributes_the_agents_change_not_the_users`，结果卡只归属本会话工具写入、不认领用户文件）。展示继续分清待审阅 / 预算让出 / 持久完成 / 恢复受阻，不能只凭 `TurnCompleted` 或 `RunCompleted` 推断业务完成。

**不要：** 全仓 `git diff HEAD` 据为 Agent 成果；diff 编辑器；自动 rollback。

---

## M16-06：上下文、GC 与搜索的实际编码闭环

**用户结果：** 跨文件切换能找回正确片段；搜索不完整不冒充全仓无命中。

**已落地：** fs.read 区间事实；同修订窗口覆盖才取代；grep/list PARTIAL；confined 有界读；错误仅 verify.run 可清。

**已全部落地（2026-09-07 复核收口）。** CONTEXT-01/WORKSPACE-01/02 已关闭；「最终曝光 ACK 与片段身份一致性」经复核已由落地提交覆盖——`7a8a663`（消费台账改由最终渲染帧重建：最终请求中被裁掉的正文不再计为已选中）、`c5f2ab7`（保守片段身份）、消费 stamping 在 release 构建同样执行（engine.rs stamping 无条件、debug_assert 仅验返回值）。不重写 Frame 编译器。

**不要：** 向量库、BM25 新服务、SIEVE/TinyLFU、learned router 作为本阶段必需。

---

## M16-07：可脚本调用的单进程运行入口

**用户结果：** 不用 TUI 也能跑有边界的任务，得到 JSONL 与诚实退出码。

**已落地：** 同一 `agent-tui`：`--prompt` / `--work` / `--continue` / `--grant-file` / `--jsonl-out`。无人审批时拒绝写入；无 `--yes`。退出码 0/2/3/1。

**仍待做：** 若 M16-02 区分了待审阅，无头 `session_end` 与之对齐；不要为脚本退出 0 把普通 final 写成 verified completion。不新增 daemon。

---

## M16-08：发布可日常试用的版本并结束本阶段

**用户结果：** 对应本次源码的 Linux/Windows 包；三类真实任务记录可审阅。

**已有：** 无头 live 三类工作区记录；空 recipe 表启动失败已修。

**PACKAGE-01 走前列（下次实际发布）。** TUI 交互走查已由 `tui_e2e_*` 覆盖（含一条带用户原有修改）。真实二进制 live 记录：**NOT_RUN**（环境无凭据；`real_binary_startup.rs` 已用 demo transport 走通真实二进制路径，记录见 `walkthroughs/2026-09-07-live-binary.md`）。诊断导出补齐 M16-05 验收：常见凭据形状防御遮蔽 + 分享前审阅声明。

**不要：** 为凑 PASS 放宽标准或扩建评测框架。M16 结束不等于 v0.2 已发布，除非另有明确发布动作。

---

## 防止再卡在测试/文档循环

- 每个工单先给出一个用户动作；测试或文档单独增加不算功能完成。
- 人工走查默认转为不依赖人的端到端测试：交互路径走会话循环 `tui_e2e_*`（脚本按键 + 帧捕获 + 真实 compose），无头路径走 `run_headless` E2E；真实 provider 的 live 记录是另一回事，不可用则 `NOT_RUN`。
- 开发跑相关回归；集成沿用现有 CI。
- 回执写清：实现了什么、是否默认产品路径、实际检查、是否真实任务、限制、下一工单。
- 安全与持久性不能靠“快点落地”绕过；真问题修当前路径。
