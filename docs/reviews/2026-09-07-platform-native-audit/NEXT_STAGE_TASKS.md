# 下一大阶段：基础、平台、正式原生 GUI 并行任务脚本

> 阶段名建议：M17（提案编号，不表示仓库已采纳）。基线 b299c6a08fdb055a0148a24dea65053861df6ff1。
> 本文件是可交给 Coding Agent 执行的任务说明，不是会自动改仓库的 shell 脚本。全部新任务状态为提案；执行前对新 HEAD 逐项确认已落地内容并跳过，不回退用户代码。

## 开工规则

先运行 `git status --short` 和 `git rev-parse HEAD`，保留用户改动。读取当前工单涉及的模块、调用者和测试；本轮没读到的模块必须现场补读，不宣称完整仓库已审查完。

三条线共享一组小契约，不各自设计任务状态。C0先约定首组消息后，B1/B2/B3与P1、G1可并行；正式GUI代码从第一批起保留。共享的contracts/command/compose入口指定单一维护者，其它线提交小接口需求，不在同一巨型文件上相互覆盖。

P3与GUI可先跑只读/有限路径；G2涉及执行与恢复保证必须等待B1/B2对应验收。基础其它研究性优化不阻塞平台和GUI。E1是平台扩展交付，不强制阻塞第一个GUI发布。

每个工单达到验收即停止扩展。下面测试命令是执行建议，本审查未执行；新项目路径和测试项目均明确为拟新增。已有测试过滤命令必须检查实际选中测试数，零测试通过不能写作验收通过。

## 概览

| 工单 | 线 | 交付 | 依赖 |
|---|---|---|---|
| C0 | shared_contract | 收敛平台＋基础＋正式 GUI 的阶段与最小契约 | 可开始 |
| B1 | foundations | 监督身份、台账、宿主验证接线与有界清理 | 可开始 |
| B2 | foundations | 处理 metadata 已发布但同步失败的围栏 | 可开始 |
| B3 | foundations | Context 验证关联与运行边界 | 可开始 |
| P1 | platform | 与 TUI 无关的原子工作提交与受理回执 | C0 |
| P2 | platform | 类型化快照、增量事件、审批与结果 | C0, P1 |
| P3 | platform | 正式 Rust 宿主与本地双向 RPC | C0, P1, P2 |
| G1 | native_gui | .NET 客户端库与正式 Avalonia 外壳 | C0 |
| G2 | native_gui | 审批、取消、审阅与恢复完整操作链 | G1, P2, P3, B1, B2 |
| G3 | native_gui | 低资源使用的正式工作台与 Context 检查 | G1, P2, B3 |
| E1 | platform_extension | 一个真实外部能力与按需 Skill 的最小闭环 | P3, G1 |
| R1 | joint_delivery | Rust＋.NET 来源绑定发布及正式使用收口 | B1, B2, B3, P3, G2, G3 |

## C0 — 收敛平台＋基础＋正式 GUI 的阶段与最小契约

**用户结果：** 不同入口使用同一操作、身份和快照语义，GUI 可直接进入长期实现。

**代码入口／拟新增路径：** `AGENTS.md`；`docs/CURRENT.md`；`docs/ROADMAP.md`；`docs/NEXT_TASKS.md`；`crates/agent-platform-protocol/src/`。

**执行步骤：**

1. 把“无工程开放项”改为具体已验范围；保留旧测试事实及新残余，不重开整个 M16。
2. 先约定 submit/continue/cancel/snapshot/subscribe/approval response 的有限 DTO、身份、错误与大小上限。
3. 把请求受理、应用、任务完成和清理确认分开；列清 supported/unsupported，不先定义所有未来 namespace。
4. 同一份契约示例供 Rust/C# 使用；共享字段修改由一个负责人合入。

**验收：**

- 已有文档检查通过。
- Rust/C# 对同一组小型消息示例的含义一致；未实现操作明确拒绝。

**建议检查（尚未执行）：**

```text
python scripts/doc_consistency.py
```

**做到这里停止：** 不新建文档治理框架；不把全部平台协议设计完作为后续开始条件。

## B1 — 监督身份、台账、宿主验证接线与有界清理

**用户结果：** 不会凭旧 PID 误杀其它进程，未确认清理不会丢监督记录，真实宿主 proof 路径应用同一监督策略。

**代码入口／拟新增路径：** `crates/tool-runtime/src/supervision.rs`；`crates/tool-runtime/src/proof_runner.rs`；`crates/tool-runtime/src/tools/process.rs`；`crates/agent-process/src/watchdog.rs`；`crates/agent-process/src/lifecycle.rs`；`crates/agent-compose/src/lib.rs`。

**执行步骤：**

1. 读取 F01–F04 对应调用链及现有 process-journal；复用稳定身份 helper，旧无身份记录默认不得 kill。
2. 台账记录/读取/结束返回 Result；限制大小与行数，串行化修改，保证必要耐久；错误不能静默当空。
3. 以明确 finished/reaped receipt 释放记录，Drop 仅承担保守清理而不是伪造确认。
4. 将监督配置贯通普通 dispatcher 与 RecipeProofRunner；不把标记 re-exec 隐式依赖传播到任意客户端 exe。
5. 修正组长、成员、正常解除语义；有界等待 watcher，无法确认则保留责任。

**验收：**

- 用自己创建的进程和错误 creation token 证明不发送错误 kill，不实际攻击别的进程。
- 损坏、不可写、读失败和 kill 无确认均不能输出清理成功。
- 从真实 Rust 宿主硬退出的 exact-proof 路径验证清理；普通通用后台进程按声明策略单独处理。
- 组长退出、成员仍活的用例符合明确监督契约。

**建议检查（尚未执行）：**

```text
cargo test -p agent-process
cargo test -p tool-runtime supervision
cargo test -p agent-compose
```

**做到这里停止：** 不新建通用 Scheduler；同一修复可以拆小 PR，但受支持的执行/恢复声明不得提前。

## B2 — 处理 metadata 已发布但同步失败的围栏

**用户结果：** 一次压缩返回不确定错误后，不会继续健康地向旧代写入。

**代码入口／拟新增路径：** `crates/agent-storage/src/lib.rs`；`crates/agent-core/src/operation.rs`。

**执行步骤：**

1. 保留 seek 前移和 Windows 替换修复；沿 helper 内部 rename→sync_directory 看错误阶段。
2. 让发布不确定显式传播或隔离 writer；显式 compact 与追加触发压缩都不能留下健康旧 writer。
3. 复用既有 fault 注入点，只新增该发布切点所需的最小注入。

**验收：**

- rename 成功且目录同步失败时，后续写明确拒绝或已安全切到新代；不得留旧代健康状态。
- 重新打开只承认已验证代际；不删除同步屏障、不选最高 gN 猜恢复。

**建议检查（尚未执行）：**

```text
cargo test -p agent-storage
cargo test -p agent-core
```

**做到这里停止：** 只修发布语义，不新增数据库、Chronicle 或另一套日志协议。

## B3 — Context 验证关联与运行边界

**用户结果：** 错误不会被同实体的无关验证终结；外部输入/输出不能在进入 Runtime 前后绕过限额。

**代码入口／拟新增路径：** `crates/context-simple/src/engine.rs`；`crates/context-simple/src/gc/reachability.rs`；`crates/agent-contracts/src/execution_facts.rs`；`crates/agent-tui/src/cli.rs`。

**执行步骤：**

1. 保留已修好的 ACK、EOF 和片段覆盖逻辑，不原样重做。
2. 对成功 verify.run 投影可信任务/故障/覆盖关系；关系不充分则只关联，不设置 VerifiedFixed。
3. stdin 与 grant 读取在读入时计费；输出 sink 不阻塞执行/取消通道，定义慢消费者和断线结果。
4. 不要把终端输入/输出 helper 直接当成正式 GUI 的客户端实现。

**验收：**

- 相同文件上不同故障/域的成功验证不终结无关故障；真正匹配的证明可终结。
- 超大/不结束输入、慢输出均有边界；没有进行全量分配之后才报超限。

**建议检查（尚未执行）：**

```text
cargo test -p context-simple
cargo test -p agent-tui
```

**做到这里停止：** 不重调 GC 阈值，不同时引入 BM25、向量或缓存算法。

## P1 — 与 TUI 无关的原子工作提交与受理回执

**用户结果：** TUI、GUI、SDK 不能把指令误投给另一客户端刚切换的任务。

**代码入口／拟新增路径：** `crates/agent-tui/src/work.rs`；`crates/agent-runtime/src/command.rs`；`crates/agent-runtime/src/actor/commands.rs`；`crates/agent-compose/src/lib.rs`。

**执行步骤：**

1. 将共享工作入口移入公共应用层；没有复用需求前不拆很多 crate。
2. 实现原子 start_work 或显式 task/expected revision 提交；复用既有 TaskManager prepare/commit。
3. 返回稳定受理身份；相同 client request id＋内容重试有界去重，异内容拒绝。
4. 明确持久范围及过期/重启 unknown：不能承诺未实现的跨重启 exactly-once，也不能自动换 ID 重放。
5. 没有可重放回执时要求先查询或人工处理未知结果；不把接收记录当成权限提升。

**验收：**

- 两个客户端交错 SetFocus/Submit 时不能跨任务投递。
- 重复请求不能偷偷再执行一次，冲突身份被拒绝。
- 原有单客户端继续任务身份不变；无需第二 TaskManager。

**建议检查（尚未执行）：**

```text
cargo test -p agent-runtime
cargo test -p agent-tui
```

**做到这里停止：** 不一次远程导出整个 RuntimeCommand，不公开恢复半事务或 CorePort。

## P2 — 类型化快照、增量事件、审批与结果

**用户结果：** 新客户端接入、重连、慢消费后可以显示正确状态，而不解析终端文字。

**代码入口／拟新增路径：** `crates/agent-runtime/src/status.rs`；`crates/agent-contracts/src/event.rs`；`crates/agent-core/src/approval.rs`；`crates/agent-tui/src/cli.rs`；`crates/agent-runtime/src/platform/`。

**执行步骤：**

1. 修任务 revision 的归属；区分 run/turn/task/closure source/connection completeness。
2. 移除从任意 ToolOutput 文本推断审批拒绝；使用可信审批结果和现有类型化事实。
3. 提供一致快照＋watermark，以及该点后的事件；定义有限重放窗口及 resync_required。
4. 实时文本流偏移与耐久事件序列分开；不能把 live-only delta 当已提交事实。
5. 审批响应绑定 request/run/operation 和认证会话，重连可查询 pending；迟到/重复应返回当前事实。
6. 慢消费者限制队列，允许合并展示进度，不允许静默抹掉审批/终态；不得阻塞 Actor。

**验收：**

- A revision9→B revision1 展示与 API 返回 B=1。
- 成功工具正文含审批拒绝短语不改变真实审批状态。
- 订阅与快照并发不漏关键事件；缺口被明确报告，安全错误不被较早普通拒绝遮盖。

**建议检查（尚未执行）：**

```text
cargo test -p agent-runtime
cargo test -p agent-tui
```

**做到这里停止：** 只构建有界可重建投影；不建 Chronicle 数据库，不让投影反向提交 effect。

## P3 — 正式 Rust 宿主与本地双向 RPC

**用户结果：** 原生 GUI 与其它入口连接同一个工作区宿主，不各自打开一份可写运行状态。

**代码入口／拟新增路径：** `crates/agent-runtime/src/platform/session.rs`；`crates/agent-process/src/session.rs`；`crates/agent-platform-protocol/src/`；`crates/agent-compose/src/lib.rs`；`proposed: crates/agent-host/`。

**执行步骤：**

1. 建立很薄的宿主可执行文件或等价现有入口；生命周期、workdir 单实例、watchdog marker 在 Rust 宿主负责。
2. Windows Named Pipe、Linux UDS 使用相同 byte framing；将 OS 特有后端隔离。先交付开发平台后补另一平台，不同时做 HTTP/TCP/gRPC。
3. 连接 ACL/peer 身份和会话授权在服务端落实；同用户不等于任意操作已获 grant，客户端不得自报 UserSteering 提权。
4. 定义帧与 decoded DOM 上限、最大并发/队列、读写期限；请求到达后可受理返回，接收循环不能等整个任务结束才读 cancel。
5. RPC 路由进入公共应用层；已有 operation query/cancel 保持语义，MCP 外部 wire 不必改成私有协议。
6. 明确关闭窗口、断线、宿主退出、task cancel 的区别；提供显式继续后台运行或停止策略。

**验收：**

- 半帧/粘帧/超大帧/无权会话正确处理；控制消息不会被日志流无限阻塞。
- 同工作区第二个客户端附着既有宿主，不产生第二个 authority writer。
- 基础 B1/B2 未完成时可开发只读连接，但不能开放相关可靠执行/恢复承诺。

**建议检查（尚未执行）：**

```text
cargo test -p agent-platform-protocol
cargo test -p agent-process
cargo test -p agent-runtime
```

**做到这里停止：** 不做系统级常驻服务、不默认公网监听，不复制 codec 和 authority。

## G1 — .NET 客户端库与正式 Avalonia 外壳

**用户结果：** 第一版就是正式原生客户端；它的通信层可被其它 .NET 应用复用。

**代码入口／拟新增路径：** `proposed: clients/dotnet/Agent.Client/`；`proposed: apps/Agent.Desktop/`；`proposed: global.json`。

**执行步骤：**

1. 以 .NET10 LTS 建立 class library 与 Avalonia app；锁定实施时确认的稳定框架/SDK版本。
2. Agent.Client 不依赖 Avalonia，不 P/Invoke 整个 Runtime；DTO 使用共同规范和跨语言样例，避免重复事实模型。
3. 实现请求关联、取消等待与显式取消命令的区别、事件流和帧大小限制；实现与目标 OS 匹配的管道/socket客户端。
4. 先用同一 DTO 的有限 fixture 驱动布局，P1/P2可用后立即连接真实宿主；fixture不是另一套模拟执行器。
5. 正式窗口包含任务选择、输入/输出、计划、状态；异步读写不占 UI 线程。

**验收：**

- client library 可在没有 GUI 的测试程序使用。
- 同一正式 GUI 连接真实 Runtime 获取快照并提交/继续任务；不替换为第二个验证 GUI。
- 没有 WebView/浏览器后端作为主界面依赖。

**建议检查（尚未执行）：**

```text
dotnet build clients/dotnet/Agent.Client/Agent.Client.csproj
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj
```

**做到这里停止：** 路径为拟新增而非已有；不先做 IDE、插件 UI SDK 或全量自绘控件库。

## G2 — 审批、取消、审阅与恢复完整操作链

**用户结果：** 用户能执行真实开发任务，并知道改动、检查、未验证状态以及可恢复点。

**代码入口／拟新增路径：** `proposed: apps/Agent.Desktop/`；`proposed: clients/dotnet/Agent.Client/`；`crates/agent-runtime/src/instance.rs`。

**执行步骤：**

1. 审批卡显示平台返回的绑定操作/范围；点击后等待真实回执，不本地宣布授权。
2. 展示工具/模型运行、预算让出、待审阅、证据完成、操作员接受和恢复受阻等不同状态。
3. 差异与工件通过授权平台按需读取；保留用户已有修改，未知归属明确标记。
4. 重开窗口走快照/事件同步；冷恢复通过完整 RuntimeInstance事务，不从文件挑字段恢复。
5. 实现中文输入法、复制、键盘操作、DPI及大文本正常行为，不仅测试截图。

**验收：**

- GUI→任务→真实工具/审批→取消/续跑→差异审阅端到端成立。
- 丢连接不会让 pending 审批自动通过；客户端崩溃不伪造任务终态。
- 一次含用户预存修改的冷恢复与继续不重复副作用。

**建议检查（尚未执行）：**

```text
dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj
cargo test -p agent-compose
```

**做到这里停止：** 不得增加第二执行器；不做全量通用代码编辑器，不因 GUI 增加绕过 Core 的文件写入口。

## G3 — 低资源使用的正式工作台与 Context 检查

**用户结果：** 长会话、大 diff 和上下文查看不会导致全历史反复传输、解析和渲染。

**代码入口／拟新增路径：** `proposed: apps/Agent.Desktop/`；`crates/agent-runtime/src/platform/`。

**执行步骤：**

1. 可视列表虚拟化同时限制底层保留数据；流式文字按小窗口合并，不每 token 重建完整Markdown/视觉树。
2. 大正文只传 locator、元数据与分页片段；视图关闭释放缓存，设置有界引用缓存。
3. Context 面板先只读显示来源、表示类型、实际曝光、片段范围和恢复状态；不显示不存在的模型内部注意力。
4. 记录全进程树空闲内存、长会话斜率、输出时CPU/分配、大diff响应；不在没有基线时宣称原生必然最省。
5. AOT/裁剪单独检查控件依赖和序列化兼容；不把AOT设为首个窗口可用的前置。

**验收：**

- 固定大型会话/差异用例的 retained data 有界，滚动不随所有历史全量重绘。
- 工具/审批/终态事件不为流畅显示而静默丢失。
- 记录环境与实际测量；没有测量的指标保持NOT_RUN。

**建议检查（尚未执行）：**

```text
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release
```

**做到这里停止：** 不建遥测平台，不写零分配通用UI，不把研究性GC算法的胜出当GUI完成标准。

## E1 — 一个真实外部能力与按需 Skill 的最小闭环

**用户结果：** 平台不只是任务入口：正式客户端能使用一个实际外围能力，Skill不常驻所有上下文。

**代码入口／拟新增路径：** `crates/agent-capability-process/src/mcp.rs`；`crates/agent-runtime/src/capability/mod.rs`；`crates/agent-runtime/src/plugin.rs`；`crates/agent-contracts/src/plugin.rs`。

**执行步骤：**

1. 先现场重读这些未在本轮完整审查的模块与测试，再复用已有 Capability/Plugin 基础。
2. 选一个只读外部能力或MCP server，配置→授权→发现→按需加载→调用→有界结果→关闭。
3. 任何被声明支持的 MCP 路径都先处理写/连接/读取消，不以“默认未开启”豁免显式使用。
4. Skill 提供有来源、版本与范围约束的按需正文读取；不提升为system权限，脚本仍由已有工具审批执行。

**验收：**

- 新增一个能力无需向RuntimeActor加入该业务特判。
- 未加载能力不把全部schema/Skill正文注入每轮请求。
- 故障和停机不隐去未确认清理。

**建议检查（尚未执行）：**

```text
cargo test -p agent-capability-process
cargo test -p agent-runtime capability
```

**做到这里停止：** 不建插件市场；不要求子Agent/DAG/递归改进才能完成本切片。

## R1 — Rust＋.NET 来源绑定发布及正式使用收口

**用户结果：** 安装包里的客户端/宿主确属本次构建，版本匹配，真实使用与验证范围明确。

**代码入口／拟新增路径：** `scripts/dist.sh`；`scripts/dist.ps1`；`.github/workflows/package.yml`；`proposed: native desktop packaging`；`docs/CURRENT.md`；`docs/COMPATIBILITY.md`。

**执行步骤：**

1. 修自定义target未传Cargo、陈旧dist混入、PowerShell退出码处理；不在旧目录拼装包。
2. 记录Rust源码SHA、.NET/协议版本、构建配置、平台依赖与checksum；不把checksum当构建来源证明。
3. 现有跨平台CI加最小C#构建/协议相容项，不重跑M15或建立新统计总门禁。
4. 正式GUI完成小bug、多文件功能、断线/冷恢复续跑；含用户原有修改。真实provider不可用写NOT_RUN。
5. 把代码实现、定向测试、默认启用、正式使用四类状态分别更新。

**验收：**

- 一个包可在声明支持环境连接对应宿主，读取不支持版本时明确拒绝。
- 源码/构建来源可追溯；失败构建不输出成功包。
- 真实检查记录完整，未跑项不被摘要成全绿。

**建议检查（尚未执行）：**

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
dotnet build apps/Agent.Desktop/Agent.Desktop.csproj -c Release
```

**做到这里停止：** 这些为集成/发布检查，不是每次改文档都执行；不要求所有未来扩展实现才发布当前正式GUI。

## 阶段后候选，不作为本阶段前置

只读工具子 Agent 可以沿平台委派接口推进：独立状态、有限预算、无递归、父操作取消传播，结果返回证据和工件。不得成为父TaskManager第二写入者，也不能另开同一可写工作区日志。先通过受限资源视图或独立快照工作区获得输入，不先做多写者并行。

Context/GC/搜索算法研究：候选质量不足再试字段化词法评分；重复正文挤预算再试依赖组边际收益；恢复成本高再试缓存准入/淘汰；维护阻塞才增脏对象与到期队列。每次只替换一个主要机制，使用既有观测记录完整任务成本，不作为GUI交付前置。

## 每项交付回执

报告：源码SHA；变更文件；用户新增动作；执行过的命令及测试数量；实际结果；默认启用状态；真实流程是否运行；未验证平台/故障；下一工单。不要把“函数存在”“单测通过”“默认接线”“真实使用通过”合并成一个Done。
