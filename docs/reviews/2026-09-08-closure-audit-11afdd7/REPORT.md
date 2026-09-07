# 源码续审与下一阶段：从“组件存在”到“正式多入口工作台可连续使用”

**仓库：** Ye-Luo-0912/context-agent-prototype  
**固定提交：** `11afdd747d6cbbb58ef0d7371841e8e365c4f8db`  
**本报告性质：** 源码静态审查＋远端 CI 观察＋少量隔离机制探针。不是完整 checkout 的全量逐行审查完成证明。

## 1. 结论与范围

本次不能声称已经读完仓库所有代码。克隆真实失败于 GitHub DNS，未取得完整源码归档；环境没有 cargo/rustc/dotnet，没有运行仓库构建或测试。读取清单精确到 **55 个路径正文**；其中新桌面应用 10 个文件、.NET 客户端/测试/互操作 19 个文件、新 Rust agent-host 5 个文件已经读取全文，共 **34 个新子树文件**。其余是基础、Runtime、协议、Context 与文档的定向全文或分段阅读。根目录及 20 个 crate 的枚举、六提交差异路径核对不等同于读过所有内容。

本次比前几轮的进展在于：**正式 GUI、客户端和宿主的新代码已完整读过，不再把它们当未实现功能，也不按旧基线重复报已修复问题。**但既有的大量 Rust 测试、评测证据、所有脚本、未展开实现不在已完成范围。详见 [机器状态](AUDIT_STATUS.json) 与 [阅读清单](READ_COVERAGE.csv)。

核心判断：保留 Rust Core/Runtime、ContextEngine、受限工具与 .NET/Avalonia 方向。下一步不是再选框架、再抽一层通用平台，而是把**长期连接、事件消费、修改回执、恢复与结果审阅**接成真实用户链路，同时并行修 Context 的终态与保留规则。

现有主要缺口分布在三处：

- 宿主长期服务：第二连接、会话释放、端点所有权、停机、真实检查点解码。
- 客户端/GUI：订阅没有数据、未知结果自动重发、连接代际、审批信息不足、轮询对象保留。
- 核心资产：决策相关性误当替代证明、lease 跨层语义不一致、Skill 文件读边界及 catalog 临时分配。

这不是“全仓全不可靠”的结论，也不应据此将全部产品冻结为新一轮全仓测试工程。各项按直接影响能力处置；不相关的优化不接管主线。

## 2. 当前事实与有效修复

当前 main 开始、结束读取均为上述 SHA；相对上轮 b299c6a 前进六个提交。远端 CI run **34148921895** 是 `failure`：Ubuntu/Windows 均停在 fmt，clippy/build 被跳过，test matrix 被跳过，docs job 成功。这不是本地重跑，也不能推导当前 SHA 的 Rust/NET 测试全部通过。[远端明细](REMOTE_CI_OBSERVED.json)

以下已在当前源码看到实际修复，不原样重开：

| 旧问题 | 当前代码事实 | 本轮证明边界 |
|---|---|---|
| 监督只记 PID、忽略写入与清理失败 | supervision 使用创建身份、类型化对账、有限台账、确认式清理；compose 拒绝 unverified/unconfirmed | 定向静态阅读，不是所有 OS 崩溃窗口通过 |
| proof runner 没继承监督策略 | compose 将 host_death_watchdog 注入 RecipeProofRunner | 已接线，不等于新 agent-host 全部生命周期通过 |
| metadata rename 后 sync 失败仍可用旧 writer | helper 返回 RecoveryRequired；compact 将 writer.failed 隔离 | 源码闭环已看到，未重跑故障注入 |
| 多 await 组成一次工作提交 | Runtime 增加 StartWork、进程内有界受理台账 | 原子提交存在；跨重启修改重发不因此安全 |
| 同实体成功验证关闭所有错误 | 当前匹配同 task 与相同 VerificationProbe，drain 时复核证据 | 不重开这个旧实体验证问题 |
| Skills 只有 metadata、MCP 无接入点 | skill_read 与 compose MCP/Plugin 配置缝已存在 | 现有代码还有 F17 文件边界；不是从零重建扩展 |

源码：[supervision](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/tool-runtime/src/supervision.rs)、[compose](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-compose/src/lib.rs#L470-L745)、[storage](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-storage/src/lib.rs#L290-L385)、[storage compact](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-storage/src/lib.rs#L650-L770)、[StartWork](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/actor/turn.rs#L1-L325)、[验证关联](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/gc/reachability.rs)。

## 3. 全链路职责检查

| 链路 | 当前应保留的所有者 | 本次关注点 |
|---|---|---|
| GUI/SDK → 本地连接 | Agent.Client＋agent-host | 会话与请求身份、连接生命周期、bounded I/O，不承载 TaskManager |
| 应用操作 → Actor | WorkControlRouter＋RuntimeHandle/StartWork | 一次操作的绑定与提交，不由前端拼接多条事实变更 |
| Actor → Core/Tools | 现有操作身份、权限、effect 和输出边界 | 不让新宿主默认路径省略原来的监督与审批 |
| 状态 → 外部客户端 | 现有 task/approval/events 的派生投影 | 快照与订阅切点、终态真实性，不解析显示文字成权威 |
| 当前焦点 → Context | 可替换 ContextEngine | 区分实体相关、语义替代、驻留和保留义务 |
| 外围扩展 → 工具面 | Capability/Plugin/MCP 适配 | 复用生命周期/权限，按需读取，不能用普通 File::open 绕过资源边界 |
| 保存 → 冷恢复 | CheckpointStore decoder＋完整 RuntimeInstance.restore | 使用正式信封而非另写 raw JSON 路径 |

当前 native GUI 不是另一个执行器，方向正确；但仅有字段、按钮和成功握手不等于上述箭头已全部接通。以下按源码逐条列出问题。

## 4. 问题清单

P1 表示直接影响安全边界、工作正确性或正式可用性的优先修复，不表示已出现事故；P2 为重要资源、输入或证据质量问题。除明确注明的隔离探针外均是静态路径结论。

### F01 · P1 · Unix 分支引用了仅在 Windows 定义的 winpipe 模块

**触发与现状：** 在非 Windows 目标编译 agent-host：serve 的 NamedPipe 分支无条件引用 winpipe::serve，模块声明却只有 #[cfg(windows)]。

**影响：** Linux 宿主构建有明确的名字解析缺口；当前 CI 停在 fmt，尚未由该 run 验证后续构建。host_e2e 的 Unix 连接辅助函数也没有真实 UDS 实现。

**最小修复：** 对两个平台提供明确 cfg 分支或受支持/不支持实现；修真实 UDS 测试连接。把 agent-host 纳入 Linux 分片及 .NET build/test 纳入现有 CI。

**必要回归：** Linux/Windows cargo check -p agent-host --all-targets；host_e2e 真正运行 UDS 与 Named Pipe，不丢弃服务线程错误；现有 .NET 客户端测试和桌面 build 在 CI 执行。

**证据与限定：** `STATIC_SOURCE; REMOTE_CI_STOPS_AT_FMT`。本轮没有 Rust 编译器。当前 CI 的实际失败是格式，不把静态编译发现伪称 CI 编译日志。  
**归属：** N0。

**源码：** [crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/lib.rs)；[crates/agent-host/tests/host_e2e.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/tests/host_e2e.rs)；[.github/workflows/ci.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/.github/workflows/ci.yml)。

### F02 · P1 · Named Pipe 每个实例都使用 FIRST_PIPE_INSTANCE

**触发与现状：** 第一条管道实例仍存在时，accept 循环再次用 FILE_FLAG_FIRST_PIPE_INSTANCE 调 CreateNamedPipeW。

**影响：** 第二个实例创建失败，accept 循环退出；已有首条连接可能继续工作，单连接冒烟不能证明可多客户端或可重连。部分拒绝分支还在 OwnedHandle 存活时直接 CloseHandle(raw)。

**最小修复：** 仅首次占用名称时使用相应独占语义，后续实例采用正常实例模式；统一 RAII 句柄所有权，拒绝分支不要手动二次关闭已拥有的句柄。

**必要回归：** 同时两个客户端分别查询；首客户端保持连接时再接入、断开、重连；拒绝/容量上限路径不双重 CloseHandle。

**证据与限定：** `STATIC_SOURCE; OFFICIAL_WIN32_CONTRACT`。保留已存在的当前用户 DACL、客户端令牌检查和拒绝远程客户端；本轮未在 Windows 执行。  
**归属：** N1。

**源码：** [crates/agent-host/src/winpipe.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/winpipe.rs)。

### F03 · P1 · 连接结束未撤销会话 grant，累计连接达到上限后 panic

**触发与现状：** build_connection_plane 每次 install 一个新 session；连接退出只 drop router，没有 revoke；累计达到会话表 64 上限。

**影响：** live connection 数量回落并不会清空授权表；在没有其他预置 grant 的条件下，第 65 次顺序连接会使 install 的 expect panic。

**最小修复：** 用连接生命周期 guard 在所有退出路径撤销该会话；install 错误作为受控拒绝，不让 accept 线程 panic。

**必要回归：** 连续至少 65 次连接/断开后授权表和句柄回到基线；坏帧、超时、鉴权失败和关闭期间同样释放；同时连接上限仍生效。

**证据与限定：** `STATIC_SOURCE`。64 是会话表上限，不是并发连接上限；不能用后者证明前者不会耗尽。  
**归属：** N1。

**源码：** [crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/lib.rs)；[crates/agent-runtime/src/platform/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/platform/work.rs)。

### F04 · P1 · 宿主没有可停止的 accept 生命周期，关闭可能无限 join

**触发与现状：** 主线程收到 Ctrl-C 后关闭 Runtime，再 join 阻塞的服务线程；accept 循环没有停止通道。服务线程提前失败时，主逻辑也不主动收敛。

**影响：** 空闲 Unix listener 可使正常退出永久停在 join；Windows 同步 Connect/Read 也需要可中断策略。宿主线程死亡可能留下仍运行而不可连接的 Runtime。

**最小修复：** 引入明确服务器停止信号和连接集合关闭；主逻辑同时观察信号与服务结果，停止新请求、结算活动操作、撤销会话、关闭传输后有界 join。

**必要回归：** 无客户端 Ctrl-C 有界退出；半帧/不读响应/待审批连接存在时仍可停机；accept 失败不静默运行到下一次 Ctrl-C。

**证据与限定：** `STATIC_SOURCE`。复用现有 RuntimeInstance.shutdown，增加的是传输生命周期，不是第二个任务调度器。  
**归属：** N1。

**源码：** [crates/agent-host/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/main.rs)；[crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/lib.rs)；[crates/agent-host/src/winpipe.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/winpipe.rs)。

### F05 · P1 · UDS 启动无条件 unlink 路径，端点所有权不成立

**触发与现状：** serve_unix 在 bind 之前对配置的 socket path 无条件 remove_file；默认名称位于全局 /tmp 固定路径。

**影响：** 可删除占位的普通文件；也可解除另一个活跃 listener 的名称，新客户端转向新 listener 而旧连接仍活。两个工作区使用相同默认名称易冲突。PID 文本锁本身也不是原子持有的 OS 锁。

**最小修复：** 采用用户私有、按 workspace/host 标识分开的端点；检查类型、所有者及活跃状态；只删除能证明属于本实例的端点；工作区单实例使用持有期 OS 锁。

**必要回归：** 普通文件占据端点时拒绝且不删除；旧 listener 活跃时拒绝接管；两个工作区的默认端点不同；同实例关闭只清理自身端点。

**证据与限定：** `STATIC_SOURCE; ISOLATED_OS_PROBE`。隔离探针只验证 unlink/bind 的 OS 行为。现有 SO_PEERCRED/chmod 及 Workspace 独占日志是缓解，未证明两个生产 Runtime 已同时写入。  
**归属：** N1。

**源码：** [crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/lib.rs)；[clients/dotnet/Agent.Client/Transports.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/Transports.cs)。

### F06 · P1 · 订阅成功但 receiver 被丢弃，GUI 收不到实际事件流

**触发与现状：** work.subscribe 返回成功后丢弃 _receiver；客户端 Dispatch 又在处理 notification 前要求 request_id；GUI 不消费运行事件流。

**影响：** 接入存在但“模型/工具输出到桌面”的主链未闭环。现有快照不携带完整输出、结果和计划，三秒轮询不能补齐缺失的数据。

**最小修复：** 保留现有 WorkEventNotification 与 PlatformEnvelope；宿主持有订阅，用单一有界 writer 复用响应与事件；客户端按 kind 验证可无 request_id 的 notification；桌面消费类型化事件并有界合并刷新。

**必要回归：** 同一真实连接收到受理、至少一个工具事件、助手结果、终态；合法 notification 不带 request_id 仍被接受；慢消费者触发明确 gap/resync 而不无限缓冲；审批/取消不被长输出阻塞。

**证据与限定：** `STATIC_END_TO_END_TRACE`。DTO 已存在，不需从零设计事件协议。轮询可以作为显式降级能力，但不能对外宣称已建立 live subscription。  
**归属：** N3。

**源码：** [crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/lib.rs)；[crates/agent-runtime/src/platform/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/platform/work.rs)；[crates/agent-platform-protocol/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-platform-protocol/src/work.rs)；[clients/dotnet/Agent.Client/AgentConnection.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/AgentConnection.cs)；[apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs)。

### F07 · P1 · 重连包装器自动重发 submit/continue/cancel

**触发与现状：** RunAsync 捕获非取消、非 AgentProtocolException 后重试 operation；Submit/Continue/Cancel 全使用它。Continue/Cancel body 为空；submit 去重仅在当前宿主进程内。

**影响：** 请求已被执行但回复丢失时，可续跑第二段或取消新的当前 turn；宿主重启后复用同 submission key 也不能保证幂等。协议违例异常也可能触发此重试。

**最小修复：** 查询与修改采用不同重试策略；未知修改结果显式返回 Unknown，先查询/重同步。修改绑定预期 task/turn/generation 与宿主 incarnation；同一实例内的已证明重复请求方可复用原回执。

**必要回归：** continue 已执行后丢响应，不再开启第二段；cancel 丢回复且焦点变化，不取消新 turn；宿主重启后 submission 不透明重发；协议错帧不触发修改重试。

**证据与限定：** `STATIC_SOURCE`。审批答复已明确不自动重试，应保留；当前 API 没有跨崩溃 exactly-once 保证，不要求新建通用幂等数据库才能先关闭危险重发。  
**归属：** N2。

**源码：** [clients/dotnet/Agent.Client/ResumableSession.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/ResumableSession.cs)；[crates/agent-platform-protocol/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-platform-protocol/src/work.rs)；[crates/agent-runtime/src/actor/turn.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/actor/turn.rs)。

### F08 · P1 · 客户端半帧写入和 Fault 状态没有关闭连接

**触发与现状：** WriteFrameAsync 已部分写入后超时/取消；SendAsync finally 只移除 pending。Dispatch 内部 Fault 只失败 pending，不标终态、不停止 reader、不关闭 stream。

**影响：** 下一帧可能接在上一半帧后，协议失步；某些已 Fault 的连接仍报告 IsConnected，后续调用继续等待或发送。

**最小修复：** 引入单一终态故障路径：原子标 Faulted、拒绝后续请求、结束通知和 pending、关闭传输以解除读；半帧写入失败必须毒化连接；仅未发送的取消和已完整发送后的等待放弃可分别处理。

**必要回归：** 短写后注入异常，后续请求不写任何字节；坏 envelope 触发 IsConnected=false 且所有 waiter 结束；请求完整写完后的本地等待取消不伪造服务端取消。

**证据与限定：** `STATIC_SOURCE`。真实 read loop 抛异常结束时本来会使 IsConnected 变 false；缺口是内部 Fault 返回和写失败路径，不笼统否定所有错误处理。  
**归属：** N2。

**源码：** [clients/dotnet/Agent.Client/AgentConnection.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/AgentConnection.cs)；[clients/dotnet/Agent.Client/Framing.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/Framing.cs)。

### F09 · P1 · 连接建立/重同步没有 single-flight、代际和一致切点

**触发与现状：** LiveAsync 锁只保护检查，connect/handshake 在锁外；fresh 在快照完成前发布。Snapshot 后 Subscribe(null)，忽略订阅回执的切点/重同步结果；Dispose 没有禁止迟到 connect 重新安装连接。

**影响：** 并发按钮/轮询可能建立并覆盖多条连接；迟到响应污染新界面；快照与事件之间存在空档；连接到另一宿主实例后无法从当前 DTO 确认同一运行世代。

**最小修复：** 每会话只允许一个连接任务；安装时校验 generation 和 disposed；快照与订阅以同一 run/host-incarnation 和有效游标衔接，缺口重新取快照，不自行拼接；失败 connect 释放资源。

**必要回归：** 并发十次 query 只进行一次 connect；connect 中 Dispose 不复活连接；切点之间产生事件，不丢不双计；重连到新宿主显示身份变化而非沿用旧状态。

**证据与限定：** `STATIC_SOURCE`。服务端 router 已有 actor barrier 和 resync-only 语义；缺口主要在 transport/client 的贯彻，不要求无限事件重放。  
**归属：** N2;N3。

**源码：** [clients/dotnet/Agent.Client/ResumableSession.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/ResumableSession.cs)；[clients/dotnet/Agent.Client/Transports.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/Transports.cs)；[crates/agent-runtime/src/platform/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/platform/work.rs)。

### F10 · P1 · 正式宿主恢复按原始 RuntimeCheckpoint JSON 读取实际信封文件

**触发与现状：** --restore-latest 枚举 JSON，按文件名选择，然后全量读入并直接 serde_json::from_str::<RuntimeCheckpoint>。CheckpointStore 实际产物是带版本/checksum 的 header + payload 信封。

**影响：** 正常保存的当前格式不能被这个原始 JSON 路径正确消费；最新文件名也不等于最近可验证检查点。不能以支持旧 raw fixture 证明真实恢复。

**最小修复：** 统一使用现有 CheckpointStore 和 decode_checkpoint_file/bytes；限制读取、验证信封及身份，从可验证记录选择 latest，再调用完整 RuntimeInstance.restore。

**必要回归：** 真实宿主保存文件后关闭、重新启动并继续原 Task；损坏/过大/不兼容信封在模型请求前明确失败；最新候选损坏时策略明确，不猜测旧格式或静默跳过。

**证据与限定：** `STATIC_SOURCE`。原始旧 JSON 可能在兼容入口仍有意义，但不能替代正式产物路径；不改写 checkpoint 格式。  
**归属：** N5。

**源码：** [crates/agent-host/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/src/main.rs)；[crates/agent-runtime/src/checkpoint.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/checkpoint.rs)；[crates/agent-runtime/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/lib.rs)。

### F11 · P2 · 多行 GUI 输入与工作协议的控制字符规则冲突

**触发与现状：** GUI AcceptsReturn=true，用户粘贴多行要求或代码；work.submit 的 validate_text 拒绝所有 char::is_control，包括 LF/CR/TAB。

**影响：** 常见输入被拒；2000 字符 goal 同时充当标题与完整指令，不符合长开发任务的输入需求。Rust scalar、C# UTF-16 length、UTF-8 byte 也存在边界口径差异。

**最小修复：** 区分短目标/显示标题与有界完整正文，复用现有用户 artifact/ref 机制；正文允许合法换行和制表，身份/路径仍严格；跨语言按契约使用标量数或 UTF-8 字节数。

**必要回归：** 中文多行要求、emoji、代码片段正常提交；正文 byte cap 在分配/持久化前检查；非法身份控制字符仍拒绝；两语言临界长度样本一致。

**证据与限定：** `STATIC_SOURCE`。不是通过删除全部输入验证来修复，也不让 GUI 自行摘要用户指令后取代原文。  
**归属：** N3。

**源码：** [apps/Agent.Desktop/MainWindow.axaml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/MainWindow.axaml)；[crates/agent-platform-protocol/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-platform-protocol/src/work.rs)；[clients/dotnet/Agent.Client/WorkDto.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/WorkDto.cs)；[clients/dotnet/Agent.Client/Envelope.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/Envelope.cs)。

### F12 · P1 · 审批与产品快照不足以支撑知情操作和结果交付

**触发与现状：** 待审批项只有 request_id 与 call_name；界面提供允许/拒绝，没有显示具体路径、argv、参数和风险。快照缺当前计划、完整结果/工件引用等关键数据。

**影响：** 用户可以操作按钮却无法在桌面确认将批准什么；执行/完成/待审阅显示缺充分事实；仅补 GUI 控件无法补出后端未提供的信息。桌面默认选择 FixtureLayout，虽然明确标注非执行器，启动窗口仍不等于进入真实工作流。

**最小修复：** 从现有审批请求提供受限详情，绑定请求身份和当前有效性；增加 GUI 实际使用的计划、执行状态、结果与工件读取投影；权限决定仍由 Core/gate 完成。

**必要回归：** 文件写入审批可看到准确路径和受限差异/参数；过期审批按钮不能授权新请求；普通 final、可信完成、取消、预算让出显示不同；结果可追溯至实际操作/工件。

**证据与限定：** `STATIC_PRODUCT_GAP`。现有审批权威没有被绕过；这是支持安全使用的界面信息缺口，不宣称所有调用是免审批执行。  
**归属：** N4;N5。

**源码：** [crates/agent-platform-protocol/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-platform-protocol/src/work.rs)；[clients/dotnet/Agent.Client/WorkDto.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/WorkDto.cs)；[apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs)；[apps/Agent.Desktop/MainWindow.axaml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/MainWindow.axaml)。

### F13 · P2 · 桌面轮询不断保留新的审批命令对象

**触发与现状：** 每次三秒刷新清空并重建审批行；每行两个命令添加进 AsyncCommandGroup 的长寿命列表但不移除。异步 timer 也未统一做 refresh single-flight/连接代际丢弃。

**影响：** 待审批持续一小时且刷新成功时，约新增 2400 个命令对象引用；Clear 可视集合不释放命令组中的引用。关闭/断开过程还没有完整挂接 ViewModel 的异步清理。

**最小修复：** 按 request_id 更新稳定行对象/命令；移除时撤销注册；刷新 single-flight、应用结果前检查连接代际；窗口退出统一取消 timer、等待任务和释放 session。输出按 bytes/chars 及行数共同限额。

**必要回归：** 同一审批刷新 1200 次后命令注册数保持有界；关闭期间慢响应不会再改 ViewModel；连续连接/断开不增加活动 timer/reader；单行大输出也受保留界限约束。

**证据与限定：** `STATIC_SOURCE; ARITHMETIC_NOT_BENCHMARK`。2400 是 2×3600/3 的路径推导，不是实测堆字节或性能报告。  
**归属：** N4;N7。

**源码：** [apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs)；[apps/Agent.Desktop/Infrastructure/ObservableObject.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/Infrastructure/ObservableObject.cs)；[apps/Agent.Desktop/App.axaml.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/App.axaml.cs)；[apps/Agent.Desktop/MainWindow.axaml.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/apps/Agent.Desktop/MainWindow.axaml.cs)。

### F14 · P2 · MetricsSession 的 whole-tree 与 idle 指标名不对应实际采样

**触发与现状：** Windows Parent() 返回 null；Linux 遍历在确认 parent 前把所有 pid 加入 seen；报告仍写 TreeWorkingSet。最后一个样本直接标为 Idle；_samples 长期追加。

**影响：** Windows 实为根进程值，Linux 可能漏掉孙进程；多个根还可能重复计数。非 idle 样本被标 idle；测量器自身持续增长。DeltaCoalescer 只有 Append 时检查时间，不保证无新输入时自动准时刷新。

**最小修复：** 覆盖范围用显式 root_only/full_tree/unknown；有界一次进程快照构建父子关系并全局去重；未实现平台返回不可用；idle 由场景标记；流式累计 count/max/last，必要诊断只存固定环。

**必要回归：** 根/子/孙进程 fixture 的覆盖完整性；Windows 未实现 parent 时不填假 whole_tree；长时间采样器保留空间固定；一次短 delta 后不再输入也按既定策略刷新。

**证据与限定：** `STATIC_SOURCE`。不据此否定其他独立人工采样，也不声称 Avalonia 比其他框架性能更差。  
**归属：** N7。

**源码：** [clients/dotnet/Agent.Client/MetricsSession.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/MetricsSession.cs)；[clients/dotnet/Agent.Client/DeltaCoalescer.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/DeltaCoalescer.cs)。

### F15 · P1 · 实体相关性被用作决策不可逆替代的证明

**触发与现状：** supersession 默认开启；带 use/switch 等词的消息被标决策，同实体/子串匹配的旧 Decision 直接排队 Superseded，未约束同 task、决策键或显式替代。

**影响：** 同一文件的两条兼容约束会互相覆盖；跨任务也可能发生。大写普通词如 Use 会被 entity 提取，进一步扩大误匹配。语义终态会使普通召回排除旧材料。

**最小修复：** 实体匹配只保留为相关性；只有明确替代目标、相同语义键及正确任务范围的事实才进入终态。无法证明时保留两条，按 attention 冷却，不把关键词分类升级为删除依据。

**必要回归：** 同文件 timeout 与 logging 两条兼容决策共存；不同任务同实体不互终结；仅共同 Use 不建立替代；明确替代保留来源 by_id 和范围。

**证据与限定：** `STATIC_SOURCE`。这是 Context 决策记忆问题，不直接等于 Runtime 的原始用户约束权威被删除；旧错误验证问题已另行修复。  
**归属：** N6。

**源码：** [crates/context-simple/src/gc/reachability.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/gc/reachability.rs)；[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/engine.rs)；[crates/context-simple/src/index/entity.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/index/entity.rs)。

### F16 · P1 · 同一有效 lease 在 Resident 与 Warm 的 TTL 结果不一致

**触发与现状：** 非 Pinned、非 latest-file 的 Ephemeral 条目，age=6、TTL=5、lease_until=10，在 turn=6 的非 AfterModel 路径：Resident TTL 直接 Tombstoned，Warm aging 先检查 lease 而跳过。

**影响：** 正文物理位置改变了语义保留义务；keep_alive 也存在同类差异，违背生命周期与驻留正交的目标。

**最小修复：** 统一跨层语义到期保护函数；单独计算 attention 和 residency。有效 lease 阻止其约定范围内的 TTL 终结，但绝不复活 Superseded/VerifiedFixed 等已证明终态。

**必要回归：** 同一合法条目跨 Resident/Warm/Stored 的 lease、keep_alive 到期矩阵；过期 lease 正常维护；已有语义终态不被 pin/lease 复活。

**证据与限定：** `STATIC_SOURCE`。需要确认 Stored 当前保留规则后复用同一策略；本轮已逐段确认 Resident 与 Warm，未跑三层完整测试。  
**归属：** N6。

**源码：** [crates/context-simple/src/residency.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/residency.rs)；[crates/context-simple/src/gc/minor.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/gc/minor.rs)。

### F17 · P1 · Skill 读取只有词法路径检查，不能限制符号链接和特殊文件

**触发与现状：** 已安装、已激活包中的相对 reference 指向 symlink/junction；skill_read 用 root.join 后普通 File::open。FIFO 在 bytes.take 生效前就可能阻塞。

**影响：** 读取可以跨出声称的包目录，或在普通文件读入口阻塞；64 KiB byte cap 不限制 open 等待与目标类型。

**最小修复：** 复用现有 ConfinedDir/受限句柄打开，固定根和句柄、拒绝不支持的链接/非普通文件，按需受限读取；结果发布前关联正确包版本/激活状态。

**必要回归：** 包内指向包外的 symlink/junction 拒绝；FIFO 命名 skill 文件时有界拒绝；合法 UTF-8/body cap/双激活门保持；更新/停用与读取交错不返回错误归属。

**证据与限定：** `STATIC_SOURCE; ISOLATED_OS_PROBE`。前提是操作者安装启用的包树存在相关文件；不是未经认证远程任意文件读取。隔离探针只读自建临时文件。  
**归属：** N6。

**源码：** [crates/agent-runtime/src/plugin.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-runtime/src/plugin.rs)。

### F18 · P2 · 有界 catalog 在截断前仍全量构造 Resident/Warm summaries

**触发与现状：** catalog 先调用返回 Vec 的 to_summaries(heap/warm)，再喂给 bounded_catalog；即使 limit=0 也先执行投影。

**影响：** 最终结果 O(limit)，但临时分配及 source/dependency 克隆是 O(Resident+Warm)；不能把 top-k 的界限宣传为整个查询的 O(limit) 额外空间。

**最小修复：** limit=0 早退；按迭代器惰性构建或先挑 ID 再克隆胜者，保留已有稳定排序；若规模仍大，再测是否需要游标/索引。

**必要回归：** limit=0 不创建逐条 summary；大量 Resident/Warm、小 limit 的分配计数受控；相同输入顺序和 tie-break 结果不变。

**证据与限定：** `STATIC_COMPLEXITY_ANALYSIS`。惰性投影不自动把扫描 CPU 降为 O(limit)；外部目录本来已经流式，不能说全部历史都已全量复制。  
**归属：** N6。

**源码：** [crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/engine.rs)；[crates/context-simple/src/heap.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/context-simple/src/heap.rs)。

### F19 · P2 · C# 的具体 payload 验证没有进入实际 SendAsync

**触发与现状：** SendAsync 验证 envelope 与 response carrier，但未调用 WorkSubmit/WorkSnapshot 等具体 payload.Validate；多数字段反序列化缺失会用默认值；成功 fixture 不覆盖这些情况。

**影响：** 声明的任务数、文本长度、task UUID 等校验与实际入口脱节；异常快照可被当作有效数据。序列化枚举默认容许数字等细节也需按既有契约审查，而非仅比较正常 ASCII 样本。

**最小修复：** 每个类型化 API 在发送前和接受响应后运行对应 validator；保持未知字段/必需字段/合法枚举/UTF-8 bytes 与 scalar 规则一致。避免反射式通用框架或为了字节顺序重写 JSON 标准。

**必要回归：** 缺 task_id、超过任务数、非法 UUID、非法 enum 分别被实际 API 拒绝；中英文/emoji 临界值两语言一致；坏响应导致协议故障且不重发修改。

**证据与限定：** `STATIC_SOURCE`。服务端有边界和同用户认证是缓解；本项不等于无限网络匿名攻击，也不否定已存在 envelope pairing。  
**归属：** N2;N3。

**源码：** [clients/dotnet/Agent.Client/AgentConnection.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/AgentConnection.cs)；[clients/dotnet/Agent.Client/Envelope.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/Envelope.cs)；[clients/dotnet/Agent.Client/ProtocolDto.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/ProtocolDto.cs)；[clients/dotnet/Agent.Client/WorkDto.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/WorkDto.cs)；[clients/dotnet/Agent.Client/JsonOptions.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client/JsonOptions.cs)。

### F20 · P2 · 部分回归名称与真正执行的路径不对应

**触发与现状：** 半帧样本 00000009 被 LE 解释为 150994944，直接走 over-cap；交错请求测试是两条连接各一个请求；interop 的 retry 使用新 key；host 线程 join 错误被忽略。

**影响：** 相关测试可能全绿却没有验证其名称暗示的半帧、同连接乱序、同 key 幂等或宿主长时间服务。

**最小修复：** 修改现有用例，不造第三套 harness：真实合法长度的半帧、单连接乱序响应、原 key 重试、执行后丢 ACK、断开重连、服务线程结果必须检查。

**必要回归：** 正确半帧应写 09000000 然后仅两字节 payload；同连接至少两个并行请求且响应逆序；同 submission key 同正文仍返回原回执；测试 server 异常使测试失败。

**证据与限定：** `STATIC_TEST_REVIEW; MECHANICAL_PROBE`。实际只运行了字节解码机械探针；没有运行这些 .NET/Rust 用例。测试问题不证明所有既有测试无价值。  
**归属：** N0;N8。

**源码：** [clients/dotnet/Agent.Client.Tests/FramingAndConnectionTests.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client.Tests/FramingAndConnectionTests.cs)；[clients/dotnet/Agent.Client.Tests/ResilienceTests.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client.Tests/ResilienceTests.cs)；[clients/dotnet/Agent.Client.Tests/FixtureConformanceTests.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/Agent.Client.Tests/FixtureConformanceTests.cs)；[clients/dotnet/InteropSmoke/Program.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/clients/dotnet/InteropSmoke/Program.cs)；[crates/agent-host/tests/host_e2e.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/11afdd747d6cbbb58ef0d7371841e8e365c4f8db/crates/agent-host/tests/host_e2e.rs)。

## 5. 优化顺序：先语义正确，再减少成本

### Context

将选择候选、最终渲染、正文片段覆盖、消费记录分别观察。GUI 为检查而打开一项材料，不应被混同为模型看过或提升模型访问热度。决策的相关性和语义终态先按 F15 修准；保留当前已经实现的同任务同 probe 验证。文件片段、旧版本与 provenance 的显示服务于实际恢复，不把模型隐藏注意力包装成分数图。

当前先做可解释的确定性修复。后续只有真正看到重复内容挤占预算，才比较依赖组“新增信息/新增 token”选择；只有词法候选与最终排序的实际缺陷有样本，才增加字段化评分。不要一边接事件一边更换所有策略。

### GC

F16 优先于评分和缓存调参。语义失效、保留义务、attention、正文 residency 保持正交；有效 lease 不应因位置变化而失效，也不能复活已证明过期的事实。当前已有分批维护与游标，不重新建增量 GC。后续按测得的维护耗时考虑 dirty-first、到期队列和有界工作配额，不新增通用 Scheduler。

### 搜索与只读检查

F18 先避免截断前全量投影。保持搜索、inspect、fetch、admit 的不同语义；为 GUI 新建的只读诊断接口不得隐式 admit。以冷读次数/字节/耗时、最终前排相关性作为改进依据。向量、SIEVE/TinyLFU、TaskGraph 都不是本阶段的验收前置。

### .NET 与原生工作台

继续当前 .NET 10/Avalonia 正式实现，不重做 Tauri 验证版。先保证有真实事件可消费、对象有明确生命周期、数据与 UI 虚拟化都受限，再讨论 AOT 或二进制传输。每个 token 重建整个输出文本、每次快照重建命令、无限保留测量样本，都是优先于框架更换的开销来源。

## 6. 下一大阶段与并行原则

建议以“**可恢复的多入口工作台：完成 M17 的真实使用闭环**”作为下一执行阶段。沿用已有目录、协议和任务身份；若项目管理需要新编号，可映射命名，但不要把 M17 已有的 DTO、宿主、客户端和 GUI 再开发一次。

[唯一建议执行队列](NEXT_STAGE_TASKS.md) 将工作分成 N0–N8。基础 N6 可与宿主 N1、客户端 N2 并行；契约由一名维护者收敛；GUI N4 围绕 N3 的真实数据一起交付。冷恢复声明等待 N5，进程/权限高风险能力等待相应修复，普通界面与读取不等所有边角审计清零。

验收单位是用户动作，而不是测试数：第二个客户端能连入；订阅后能看到一次真实工具结果；未知续跑结果不会自动重发；多行指令可提交；审批能查看具体对象；同一任务可从正式检查点重开；长会话不会持续累积已过期 UI 对象。

## 7. 文档调整

只调整现有 CURRENT/NEXT_TASKS/ROADMAP/ARCHITECTURE/AGENTS 的活动说明，不新增平行文档治理系统。

- 将“已落地”细分为代码存在、已接真实传输、已接正式客户端、已运行具体平台场景。P3/G2/G3 的库或外壳状态不再替代全链验收。
- CURRENT 的旧开工段和下方新完成段合并。B3 的 task/probe 身份关联已经比文档所说更完整，应更新；未做的事件传递/结果接口不要关闭。
- ARCHITECTURE 仍写本项目将由 ContextCore 提供能力，且入口仍为 TUI now。用户已明确两项目独立，应修目的段和部署图；不要因此无计划重命名已有兼容 crate。
- 对当前 SHA 引用当前 CI。历史 b299c6a 全绿不能当作新宿主与 .NET 客户端的验证。
- 附件中的 Chronicle→TaskGraph 阶梯保留为历史提案，不重新变成活动门禁；不重开 M15，不改写 evidence。

## 8. 本轮实际执行与未执行

实际执行：克隆尝试；GitHub 固定源码/差异/CI 查询；读取清单中的源码；Python 隔离 UDS unlink/rebind、普通路径删除、包内 symlink 读取机制、LE 半帧样本解码和常量摘要核对。

未执行：仓库 cargo check/test/clippy、dotnet build/test、真实 Windows Named Pipe、真实桌面 IME/DPI、真实模型开发任务、生产 checkpoint 恢复、内存/性能 benchmark、全仓全部源码逐行审查。

[探针原始结果](LOCAL_PROBES.json) 不可被引用为生产用例通过；[脚本](probe_local_boundaries.py) 只操作自己创建的临时对象。报告并未修改 GitHub 或本地用户仓库。
