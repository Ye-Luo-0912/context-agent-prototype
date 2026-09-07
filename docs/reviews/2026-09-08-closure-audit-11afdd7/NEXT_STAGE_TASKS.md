# 下一阶段可执行任务：可恢复的多入口 Agent 工作台

**状态：提案，未实施、未向 GitHub 推送。**  
**源码依据：** `11afdd747d6cbbb58ef0d7371841e8e365c4f8db`，部分全仓审查；完整读取 apps/clients/agent-host 的新增子树。  
**替换关系：** 本队列接续现有 M17 未闭环项；不是再加 N0–N8 作为原队列全部任务的前置，也不是重做已落地的 B1/B2/C0/P1/G1/E1。

## 目标

同一正式运行实例可通过原生 GUI、TUI/SDK 使用：输入能够提交、实际输出能够到达、审批能够知情、取消与续跑不会误操作、断开可重连、正式检查点可恢复、结果可审阅、长会话占用有界。

继续使用当前 Rust Core/Runtime、独立 agent-host、.NET 10/Avalonia 和现有 PlatformEnvelope/长度前缀传输。该项目独立于 ContextCore。TUI 是一个客户端，不是权威；GUI 不是第二个执行器；不引入 Chronicle、TaskGraph、通用 Scheduler 或另一份 TaskManager。

## 执行约定

1. 先读 CURRENT 与本队列当前任务，再读涉及的源码、调用方和已有测试。初始运行 `git status --short`、`git rev-parse HEAD`；源码已更新时做差异复核，禁止回退/覆盖用户修改。
2. 本报告的问题只针对固定 SHA。已修复项定向确认后跳过；未读取模块在动手前补读，不能用本报告当作全仓认证。
3. 一次小功能或同一故障边界一个提交。开发时只跑相关测试，合并复用现有 CI；相同源码相同命令无新原因不反复跑。
4. 下面命令是 Coding Agent 应执行的建议，**本轮并未执行这些 Rust/.NET 命令**。测试过滤匹配零用例不是通过；真实 provider 不可用写 NOT_RUN。
5. 共用协议/RuntimeCommand/compose 由单一负责人合入变更。基础与 UI 可并行，但不同时各写一套身份、状态或协议。
6. 每次回执分开记录：代码是否存在、是否已接真实产品路径、运行了哪些检查、真实任务是否完成、支持范围限制。不要用测试数和文档条目代替用户动作。

## 并行与进入顺序

| 批次 | 可以开工的工作 | 约束 |
|---|---|---|
| 第一批 | N0 最小构建修复；N1 宿主；N2 客户端；N6 基础语义与文件边界 | N1/N2/N6 的代码工作可在 N0 集成检查期间并行，不等全仓人工复测 |
| 第二批 | N3 真实订阅/输入；N4 正式 GUI；N5 恢复/工件 | 先固定正在使用的最小 DTO，GUI 从第一条真实事件链开始 |
| 第三批 | N7 长会话；N8 扩展配置与发布 | 高风险恢复/执行必须满足其对应修复；不相关研究问题不阻止展示切片交付 |

N1–N5 不是为每个入口再建运行器，而是共用一个 Actor 和事实来源。网络请求不能直接成为 RuntimeEvent 完成事实。修改结果未知时不重发；本地连接身份不等于任意操作权限。

## N0 — 恢复当前支持平台构建与准确验证入口

**工作线：** 集成/契约  
**集成依赖：** 无；立即开始  
**问题映射：** F01, F20  
**源码入口：** `crates/agent-host/src/lib.rs`, `crates/agent-host/tests/host_e2e.rs`, `.github/workflows/ci.yml`, `docs/CURRENT.md`, `docs/NEXT_TASKS.md`

### 实施步骤
1. 先 git status/rev-parse；工作树已改过则按源码定向复核，不覆盖用户修改或回退到审查 SHA。
2. 修 cfg 路径和 Unix 连接辅助函数；现有 CI 增补 agent-host Linux 与 .NET 客户端/桌面检查；只做必要 fmt。
3. 修现有半帧、同连接乱序、同 key 重试测试；服务线程错误必须传给用例。
4. 活动队列切到本文的实际动作，已实现部分保留；只改相关文档，不清洗整个 archive。

### 完成标准
- 支持的 Linux/Windows 目标均编译宿主；.NET build/test 真正被 CI 运行。
- 任何未执行用例明确 NOT_RUN，测试名称与实际故障切点一致。

### 相关命令（需在目标环境执行）

```bash
git status --short
git rev-parse HEAD
git diff --check
cargo fmt --all -- --check
cargo check -p agent-host --all-targets --locked
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj
```

**做到这里停止：** 不新建评测框架；不等所有历史 Rust/eval 用例人工重复跑完才开 N1/N2/N6。

## N1 — 宿主可长期接入、可安全关闭

**工作线：** 平台  
**集成依赖：** N0  
**问题映射：** F02, F03, F04, F05  
**源码入口：** `crates/agent-host/src/main.rs`, `crates/agent-host/src/lib.rs`, `crates/agent-host/src/winpipe.rs`, `crates/agent-runtime/src/platform/work.rs`

### 实施步骤
1. 修 Named Pipe 首实例策略与句柄唯一所有权；在一个活跃实例存在时仍能接下一客户端。
2. 将 session grant 归属连接 guard，断线/坏帧/超时统一 revoke；用错误替代 expect panic。
3. 明确服务停止信号、当前连接集合和服务失败回执；停止接收与 Runtime 关闭有明确顺序，有界 join。
4. UDS 使用用户私有、按工作区区分的端点；预存普通文件/他人 listener 拒绝覆盖；只清理本实例拥有的端点。
5. 保留本地认证和权限限制；传输回路不是 Task 调度器。

### 完成标准
- 两个并行客户端及至少 65 次顺序重连，授权表/连接数回落。
- 无客户端、半帧客户端、慢读客户端存在时均可有界停止。
- 端点占用与服务线程失败被调用方明确看到。

### 相关命令（需在目标环境执行）

```bash
cargo test -p agent-host --test host_e2e
cargo test -p agent-runtime work_control
```

**做到这里停止：** 不做公网 TCP/HTTP、系统服务安装器、多工作区调度；测试过滤必须报告实际匹配数，零个不算通过。

## N2 — 未知修改结果不重发，连接状态不复活

**工作线：** 客户端/共享操作语义  
**集成依赖：** N0  
**问题映射：** F07, F08, F09, F19  
**源码入口：** `clients/dotnet/Agent.Client/AgentConnection.cs`, `clients/dotnet/Agent.Client/ResumableSession.cs`, `clients/dotnet/Agent.Client/Transports.cs`, `clients/dotnet/Agent.Client/Envelope.cs`, `clients/dotnet/Agent.Client/WorkDto.cs`, `crates/agent-runtime/src/platform/work.rs`, `crates/agent-platform-protocol/src/work.rs`

### 实施步骤
1. 把查询重试与修改未知结果分开：立即停止透明重发 continue/cancel/跨宿主 submit。
2. 修改携带预期 task/turn/generation 与 host incarnation，回执返回关联身份；沿用现有进程内 receipt，不谎称跨崩溃 exactly-once。
3. single-flight connect，handshake 成功后才安装连接；连接代际/Dispose 防止迟到任务复活旧连接。
4. 将 Fault 实现为真正终态；部分写入失败关闭连接，拒绝新请求，完成所有 waiters；同一坏连接不重用。
5. 每个公开 API 在实际发送/接受路径运行 payload 验证；限定 pending 数与通知累计字节。

### 完成标准
- 真实修改执行后丢响应，不产生第二次操作；查询可按声明安全重试。
- 并发连接请求只有一条新连接；Dispose 中的迟到连接释放。
- 坏帧/短写后 IsConnected 为 false；异常类型不触发修改重放。

### 相关命令（需在目标环境执行）

```bash
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj --filter "FullyQualifiedName~ConnectionTests|FullyQualifiedName~ResumableSessionTests"
cargo test -p agent-platform-protocol work
```

**做到这里停止：** 不增加通用持久 RPC 数据库；在无法证明时返回 Unknown/需重同步，不为了退出成功而猜测。

## N3 — 同一连接上的事件、快照和完整输入

**工作线：** 平台/客户端契约  
**集成依赖：** N1, N2  
**问题映射：** F06, F09, F11, F19  
**源码入口：** `crates/agent-host/src/lib.rs`, `crates/agent-runtime/src/platform/work.rs`, `crates/agent-platform-protocol/src/work.rs`, `clients/dotnet/Agent.Client/AgentConnection.cs`, `clients/dotnet/Agent.Client/IAgentConnection.cs`, `clients/dotnet/Agent.Client/ResumableSession.cs`

### 实施步骤
1. 复用现有 WorkEventNotification，保持 receiver 到连接关闭；响应和通知共用单一有界 writer，不互相交错字节。
2. 按 kind 分开验证 request/response/notification；notification 不要求 request_id。事件需要 run 身份、游标及 live-only 的明确定义。
3. 固定 snapshot+subscribe 的一致切点；使用现有 barrier/resync-only，缺口返回 resync_required。先不承诺持久重放；禁止 Snapshot 后忽略 Subscribe 回执。
4. 把工作短标题与完整用户正文区分；复用已有有界正文/artifact，引入该客户端真正需要的多行输入与追加入口，控制字符规则按字段定义。
5. 协议修改更新受支持 profile/fixture，不为跨语言更换整套现有长度前缀 transport。

### 完成标准
- 真实 Rust 宿主 → .NET 客户端：受理、工具、助手内容和终态均可接收。
- 切点期间产生事件不丢不双计；重连至新宿主不能沿用旧游标。
- 多行代码/中文/emoji 在合法边界接受，超过 cap 在读入时拒绝。

### 相关命令（需在目标环境执行）

```bash
cargo test -p agent-host --test host_e2e
cargo test -p agent-platform-protocol work
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj
```

**做到这里停止：** 不新建 Chronicle；不将每个 native 同进程调用序列化；不要求 MCP server 改说内部 PlatformEnvelope。

## N4 — 从按钮和快照变成真正的任务操作面

**工作线：** 正式 GUI  
**集成依赖：** N3  
**问题映射：** F06, F12, F13  
**源码入口：** `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`, `apps/Agent.Desktop/Infrastructure/ObservableObject.cs`, `apps/Agent.Desktop/MainWindow.axaml`, `apps/Agent.Desktop/App.axaml.cs`, `clients/dotnet/Agent.Client/IAgentConnection.cs`, `crates/agent-platform-protocol/src/work.rs`

### 实施步骤
1. 连接/加载/断开状态以真实握手结果为准；后台单消费者读事件，UI 线程合并显示；慢请求有明确 busy 与取消等待表现。
2. 审批由既有 gate 提供 request_id、路径/argv、风险与有界参数详情；根据真实 Delivered/NoLongerPending 更新，不本地制造允许事实。
3. 按 request_id/task_id 复用稳定行 ViewModel；刷新 single-flight；移除审批时解除命令注册；窗口关闭按生命周期释放资源。
4. 提供真实短计划、open loops、当前执行状态与结果卡；缺路由则明示 unavailable，不从通用文字反推。
5. 当前默认 FixtureLayout；保留 fixture 只作显式布局预览开关，正式默认通道改为真实宿主或明确的连接配置，不把安装后的首次体验留在演示数据；不再另建验证 GUI。

### 完成标准
- 提交 → 工具/输出 → 审批 → 让出/取消 → 继续 → 产出待审是一条正式链。
- 一项待审批反复刷新后注册命令数量不增长。
- 旧连接慢响应不污染新连接/新任务；窗口关闭不残留轮询。

### 相关命令（需在目标环境执行）

```bash
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj
```

**做到这里停止：** 不做 IDE/编辑器平台；不在 GUI 保存另一份任务、权限或完成真相。必要 GUI 生命周期用现有测试方式新增定向用例。

## N5 — 正式检查点恢复与可审阅工件

**工作线：** 平台/正式 GUI  
**集成依赖：** N2, N3  
**问题映射：** F10, F12  
**源码入口：** `crates/agent-host/src/main.rs`, `crates/agent-runtime/src/checkpoint.rs`, `crates/agent-runtime/src/lib.rs`, `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`, `crates/agent-platform-protocol/src/work.rs`

### 实施步骤
1. 恢复统一使用 CheckpointStore 的受限验证解码与完整 RuntimeInstance.restore，不再按 raw JSON 或文件名字典序猜测。
2. 宿主与 TUI 的 model/context/监督/approval profile 来源明确且显示身份；允许不同策略，但不把 Rolling 宿主视为已验证 Dynamic 产品的等价替身。
3. 用现有变更/工件事实提供结果、差异与检查读取；只读调用不新建模型回合，不把整个 git diff 归为 Agent 修改。
4. GUI 明确区分断开窗口、取消任务、停止宿主、恢复后继续；默认生命周期在文档和代码一致。

### 完成标准
- 同一正式宿主生成的信封重启后恢复原 task，并显式继续。
- 损坏/过大/错误 profile 在任何新执行前被拒；未知效果继续围栏。
- 结果展示已知 Agent 改动、用户原有改动/未知归属、真实执行检查与未测项。

### 相关命令（需在目标环境执行）

```bash
cargo test -p agent-host --test host_e2e
cargo test -p agent-compose
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj
```

**做到这里停止：** 不改变旧检查点格式或创建新的恢复引擎；不承诺所有 GUI 关闭形式都会后台持续运行。

## N6 — 修准语义保留、只读成本与 Skill 路径

**工作线：** 基础核心/扩展边界  
**集成依赖：** N0  
**问题映射：** F15, F16, F17, F18  
**源码入口：** `crates/context-simple/src/gc/reachability.rs`, `crates/context-simple/src/engine.rs`, `crates/context-simple/src/residency.rs`, `crates/context-simple/src/gc/minor.rs`, `crates/context-simple/src/heap.rs`, `crates/agent-runtime/src/plugin.rs`

### 实施步骤
1. 取消仅实体重合就不可逆终结旧 Decision 的规则；复用显式替代/正确任务范围；不误改已经修好的 VerificationProbe 链。
2. 提取 Resident/Warm/Stored 共用的到期保护判断，明确 lease/keep_alive 的保护范围与终态不可复活。
3. catalog 早退 limit0，按 lazy projection 或选中 ID 后复制，保持顺序语义；只读 GUI inspect 不应被当成模型消费。
4. Skill 使用现有 ConfinedDir/受限普通文件句柄、根路径固定和有界读取，覆盖 symlink/junction/FIFO；保留版本、来源、双激活门。
5. 拆成数个相关小 PR，不把这四处修复扩成整个 ContextEngine 重写。

### 完成标准
- 同文件两条兼容决策、跨任务相同实体均可保留。
- 有效租约在不同 residency 中不产生不同语义终态。
- limit0 不分配全部 summary；合法排序回归保持。
- 包外链接/特殊文件拒绝，合法 Skill 可通过现有工具读取。

### 相关命令（需在目标环境执行）

```bash
cargo test -p context-simple
cargo test -p agent-runtime --lib plugin::
```

**做到这里停止：** 不增加向量、学习排序、TaskGraph 或新 GC 策略；本组可与 N1/N2/GUI 展示并行，不接管全部任务。

## N7 — 长会话有界而且指标可解释

**工作线：** GUI/测量  
**集成依赖：** N4  
**问题映射：** F13, F14  
**源码入口：** `clients/dotnet/Agent.Client/MetricsSession.cs`, `clients/dotnet/Agent.Client/DeltaCoalescer.cs`, `apps/Agent.Desktop/ViewModels/MainWindowViewModel.cs`, `apps/Agent.Desktop/MainWindow.axaml`

### 实施步骤
1. 输出缓存同时按 chars/bytes/rows 限制；流式显示增量而非持续重建全部历史；需要的长工件从宿主按需读取。
2. DeltaCoalescer 由真实消费循环驱动有界 timer/末尾 flush，回调不在不必要的大锁内执行；不将 UI 节流用于延迟控制请求。
3. MetricsSession 按覆盖范围报告 root_only/full_tree/unknown，修 Linux parent 遍历，Windows未实现不写假全树值；去重多个root。
4. 只维护有界采样环或流式 max/count/last；明确 idle 场景；关闭时等待 sampler 并释放句柄。

### 完成标准
- 长会话命令、pending、事件、文本、采样缓存均回落或保持固定界限。
- 采样未覆盖的部分标 Unknown/NOT_RUN，不填零、不改名当全树。
- 真实 GUI 的 CJK IME/DPI/复制/大差异响应逐项记录，不制造普遍性能结论。

### 相关命令（需在目标环境执行）

```bash
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj --filter "FullyQualifiedName~MetricsSessionTests|FullyQualifiedName~DeltaCoalescerTests"
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release
```

**做到这里停止：** 不把 Native AOT、零拷贝或共享内存列为首次发布前置；不新增资源指标数据库。

## N8 — 把现有能力接入产品并完成来源绑定发布

**工作线：** 扩展/联合交付  
**集成依赖：** N1, N2, N3, N4, N5, N6  
**问题映射：** F20  
**源码入口：** `crates/agent-compose/src/lib.rs`, `crates/agent-host/src/main.rs`, `crates/agent-runtime/src/plugin.rs`, `crates/agent-capability-process/src/mcp.rs`, `scripts/dist.sh`, `scripts/dist.ps1`, `.github/workflows/package.yml`

### 实施步骤
1. 实施前补读本轮未展开的 MCP、发布脚本及调用方。已有 E1 配置/目录/Skill读取复用，不再写另一套 registry。
2. 给正式宿主暴露受限的明确 MCP/Plugin 配置路径和 supported/unsupported 信息；启用来源是可信操作者配置，不把安装当授权；模型输出不能设置权限。
3. 既有跨语言夹具补成真正 .NET → Rust host → Runtime → Tool → Event → GUI 的用例；使用现有 scripted model 验证确定性动作，真实 provider 走查单列。
4. 构建并打包当前 Rust 宿主、桌面与必要依赖，干净 staging 和一致 source/config 身份；发布脚本旧问题如仍存在在本次打包时修。
5. 三类真实任务：小 bug、两三文件功能、跨文件修改后中断恢复；保留用户原有修改；未执行的 live 项写 NOT_RUN。

### 完成标准
- 安装后连接真实宿主而非 fixture；已有 MCP/Skill 通过目录和权限边界使用。
- 支持平台上的包来自同一源码与明确 profile；宿主与客户端互操作证据同包可定位。
- 三个任务记录有产物/检查/未验证范围，不据此宣称通用成功率。

### 相关命令（需在目标环境执行）

```bash
cargo test -p agent-host --test host_e2e
cargo test -p agent-capability-process
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release
```

**做到这里停止：** 不创建新 release/tag，除非用户明确要求；不做插件市场、递归自修改或并行写入子 Agent。本切片完整验收前不写整个 M17 已完成。

## 交付给 Coding Agent 的起始指令

先核对当前 HEAD 与未提交修改。读取当前任务和涉及源码，确认本报告的路径是否仍成立。立即处理 N0 的 fmt/cfg/验证入口最小修复，同时可以分别开 N1 的宿主长期连接、N2 的客户端未知结果处理、N6 的 Context/Skill 边界。不要重做已落地的 Runtime 原子 StartWork、身份化监督、metadata 发布围栏和 VerificationProbe 关联。公共契约单一维护者。第一条联合交付是正式 GUI 从真实 agent-host 接到工具/助手输出；不新增验证 GUI，不以 TaskGraph/新数据库/通用框架为前置。每项达到完成标准即停止扩展，报告实际命令、支持范围和下一工单。

## 阶段后候选，不作为前置

工具子 Agent 首先限只读、独立状态、有限预算、无递归、受父调用取消；不能直接打开第二份同工作区可写权威。复杂执行图仅在真实任务显示简单 plan/open_loops/next_action 不足时讨论。Context/GC/搜索算法优化一次只改一个主要机制：先有正确候选/来源/消费反馈，再看字段化排序、边际 token 选择、正文缓存或 dirty-first 维护的实际收益。
